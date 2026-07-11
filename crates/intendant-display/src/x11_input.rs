//! In-process X11 input injection, capture, and clipboard via x11rb + XTest.
//!
//! Replaces the previous subprocess-per-action path (`xdotool` for input,
//! ImageMagick `import` for screenshots, `xclip` for paste): one persistent X
//! connection per display, no fork/exec per action, no external binaries to
//! install on agent hosts.
//!
//! Key identity matches the session backends: chord/key actions arrive as DOM
//! physical-key codes (produced by `key_action_events`) and map through
//! [`crate::keymap::dom_code_to_x11_keycode`] (evdev + 8, the
//! universal offset on evdev-based Xorg/Xvfb). `type_text` is
//! layout-independent: each character resolves to its keysym through the
//! server's *actual* keyboard mapping when present, else through a temporarily
//! remapped scratch keycode (the classic xdotool technique).
//!
//! Linux-only (x11rb is a Linux-target dependency); `computer_use` routes to
//! stubs elsewhere. Everything speaks blocking X protocol I/O, so public
//! functions wrap the work in `spawn_blocking`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::keymap::{char_to_x11_keysym, dom_code_to_x11_keycode};

/// Delay between the events of a key chord / repeated clicks, matching the
/// session-backend pacing so apps see human-plausible sequences.
const KEY_EVENT_GAP: Duration = Duration::from_millis(10);
const MULTI_CLICK_GAP: Duration = Duration::from_millis(50);
const SCROLL_TICK_GAP: Duration = Duration::from_millis(20);
const DRAG_STEP_GAP: Duration = Duration::from_millis(20);
const DRAG_STEPS: i32 = 5;
/// How long `paste` waits after injecting ctrl+v for the target app to fetch
/// the selection (the event thread keeps serving afterwards regardless).
const PASTE_TRANSFER_WINDOW: Duration = Duration::from_millis(500);
/// Refuse pastes larger than this rather than implementing INCR transfers.
const PASTE_MAX_BYTES: usize = 1 << 20;

// ── Connection cache ─────────────────────────────────────────────────────────

struct Atoms {
    clipboard: xproto::Atom,
    utf8_string: xproto::Atom,
    targets: xproto::Atom,
    text_plain_utf8: xproto::Atom,
}

/// Text currently being served as the CLIPBOARD selection (if any).
struct ClipboardServing {
    text: Arc<Vec<u8>>,
}

struct DisplayConn {
    conn: RustConnection,
    root: xproto::Window,
    min_keycode: u8,
    max_keycode: u8,
    /// Hidden input-only window that owns the CLIPBOARD selection for paste.
    selection_window: xproto::Window,
    atoms: Atoms,
    serving: Mutex<Option<ClipboardServing>>,
    /// keysym -> (keycode, needs_shift), built lazily for `type_text`.
    keysym_index: Mutex<Option<KeysymIndex>>,
    /// Serializes paste operations per display.
    paste_lock: Mutex<()>,
}

struct KeysymIndex {
    map: HashMap<i32, (u8, bool)>,
    shift_keycode: Option<u8>,
    /// A keycode with no keysyms bound, usable for temporary remapping.
    scratch_keycode: Option<u8>,
}

/// Internal error split so `with_conn` can distinguish a dead connection
/// (drop from cache, retry once) from an operation-level failure.
enum OpError {
    Conn(String),
    Other(String),
}

impl From<x11rb::errors::ConnectionError> for OpError {
    fn from(e: x11rb::errors::ConnectionError) -> Self {
        OpError::Conn(format!("X11 connection error: {e}"))
    }
}

impl From<x11rb::errors::ReplyError> for OpError {
    fn from(e: x11rb::errors::ReplyError) -> Self {
        match e {
            x11rb::errors::ReplyError::ConnectionError(c) => c.into(),
            other => OpError::Other(format!("X11 request failed: {other}")),
        }
    }
}

impl From<x11rb::errors::ReplyOrIdError> for OpError {
    fn from(e: x11rb::errors::ReplyOrIdError) -> Self {
        match e {
            x11rb::errors::ReplyOrIdError::ConnectionError(c) => c.into(),
            other => OpError::Other(format!("X11 request failed: {other}")),
        }
    }
}

fn cache() -> &'static Mutex<HashMap<String, Arc<DisplayConn>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<DisplayConn>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn invalidate(display: &str) {
    cache().lock().unwrap().remove(display);
}

fn connect_cached(display: &str) -> Result<Arc<DisplayConn>, String> {
    if let Some(dc) = cache().lock().unwrap().get(display) {
        return Ok(dc.clone());
    }
    let dc = Arc::new(connect(display)?);
    // Dedicated event thread per connection: the single consumer of the X
    // event queue. It answers clipboard SelectionRequests (paste serving) and
    // exits when the connection dies. x11rb routes replies to their waiting
    // requesters independently of this queue, so blocking here is safe.
    let event_dc = dc.clone();
    std::thread::Builder::new()
        .name(format!("x11-input-events-{display}"))
        .spawn(move || event_loop(event_dc))
        .map_err(|e| format!("spawn X11 event thread: {e}"))?;
    cache()
        .lock()
        .unwrap()
        .insert(display.to_string(), dc.clone());
    Ok(dc)
}

fn connect(display: &str) -> Result<DisplayConn, String> {
    let (conn, screen_num) = RustConnection::connect(Some(display))
        .map_err(|e| format!("cannot connect to X display {display}: {e}"))?;
    let setup = conn.setup();
    let min_keycode = setup.min_keycode;
    let max_keycode = setup.max_keycode;
    let screen = setup
        .roots
        .get(screen_num)
        .ok_or_else(|| format!("X display {display} has no screen {screen_num}"))?;
    let root = screen.root;

    // Verify the XTEST extension is present before claiming input support.
    conn.xtest_get_version(2, 2)
        .map_err(|e| format!("XTEST version request failed: {e}"))?
        .reply()
        .map_err(|e| format!("X display {display} lacks the XTEST extension: {e}"))?;

    let clipboard = intern_atom(&conn, "CLIPBOARD")?;
    let utf8_string = intern_atom(&conn, "UTF8_STRING")?;
    let targets = intern_atom(&conn, "TARGETS")?;
    let text_plain_utf8 = intern_atom(&conn, "text/plain;charset=utf-8")?;

    // Hidden input-only window: selection owner for paste. Never mapped.
    let selection_window = conn
        .generate_id()
        .map_err(|e| format!("allocate window id: {e}"))?;
    conn.create_window(
        0, // depth: CopyFromParent
        selection_window,
        root,
        -1,
        -1,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_ONLY,
        0, // visual: CopyFromParent
        &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| format!("create selection window: {e}"))?;
    conn.flush()
        .map_err(|e| format!("flush after connect: {e}"))?;

    Ok(DisplayConn {
        conn,
        root,
        min_keycode,
        max_keycode,
        selection_window,
        atoms: Atoms {
            clipboard,
            utf8_string,
            targets,
            text_plain_utf8,
        },
        serving: Mutex::new(None),
        keysym_index: Mutex::new(None),
        paste_lock: Mutex::new(()),
    })
}

fn intern_atom(conn: &RustConnection, name: &str) -> Result<xproto::Atom, String> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(|e| format!("intern {name}: {e}"))?
        .reply()
        .map_err(|e| format!("intern {name}: {e}"))?
        .atom)
}

/// Run a blocking op against the cached connection for `display`, retrying
/// once with a fresh connection when the cached one turns out dead (X server
/// restarted between calls).
async fn with_conn<T, F>(display: &str, op: F) -> Result<T, String>
where
    T: Send + 'static,
    F: Fn(&DisplayConn) -> Result<T, OpError> + Send + Sync + 'static,
{
    let display = display.to_string();
    tokio::task::spawn_blocking(move || {
        let mut last_err = None;
        for attempt in 0..2 {
            let dc = connect_cached(&display)?;
            match op(&dc) {
                Ok(v) => return Ok(v),
                Err(OpError::Conn(e)) if attempt == 0 => {
                    invalidate(&display);
                    last_err = Some(e);
                }
                Err(OpError::Conn(e)) | Err(OpError::Other(e)) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| "X11 operation failed".to_string()))
    })
    .await
    .map_err(|e| format!("X11 input task join: {e}"))?
}

// ── Low-level XTest primitives (blocking) ────────────────────────────────────

fn fake_motion(dc: &DisplayConn, x: i32, y: i32) -> Result<(), OpError> {
    dc.conn.xtest_fake_input(
        xproto::MOTION_NOTIFY_EVENT,
        0, // absolute
        x11rb::CURRENT_TIME,
        dc.root,
        x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        0,
    )?;
    Ok(())
}

fn fake_button(dc: &DisplayConn, button: u8, press: bool) -> Result<(), OpError> {
    let kind = if press {
        xproto::BUTTON_PRESS_EVENT
    } else {
        xproto::BUTTON_RELEASE_EVENT
    };
    dc.conn
        .xtest_fake_input(kind, button, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)?;
    Ok(())
}

fn fake_key(dc: &DisplayConn, keycode: u8, press: bool) -> Result<(), OpError> {
    let kind = if press {
        xproto::KEY_PRESS_EVENT
    } else {
        xproto::KEY_RELEASE_EVENT
    };
    dc.conn
        .xtest_fake_input(kind, keycode, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)?;
    Ok(())
}

/// Round-trip barrier: guarantees the server has processed everything sent so
/// far (the in-process equivalent of `xdotool --sync`).
fn sync(dc: &DisplayConn) -> Result<(), OpError> {
    dc.conn.get_input_focus()?.reply()?;
    Ok(())
}

fn keycode_for(dc: &DisplayConn, code: &str) -> Result<u8, OpError> {
    let kc = dom_code_to_x11_keycode(code)
        .ok_or_else(|| OpError::Other(format!("unsupported key code: {code}")))?;
    if kc < dc.min_keycode || kc > dc.max_keycode {
        return Err(OpError::Other(format!(
            "key code {code} maps to X11 keycode {kc}, outside the server's \
             {}..={} range",
            dc.min_keycode, dc.max_keycode
        )));
    }
    Ok(kc)
}

// ── Public input operations ──────────────────────────────────────────────────

/// Move the pointer and click `clicks` times with the given X11 button
/// (1=left, 2=middle, 3=right).
pub async fn click(display: &str, x: i32, y: i32, button: u8, clicks: u32) -> Result<(), String> {
    with_conn(display, move |dc| {
        fake_motion(dc, x, y)?;
        sync(dc)?;
        for i in 0..clicks.max(1) {
            if i > 0 {
                std::thread::sleep(MULTI_CLICK_GAP);
            }
            fake_button(dc, button, true)?;
            fake_button(dc, button, false)?;
            dc.conn.flush()?;
        }
        sync(dc)
    })
    .await
}

pub async fn mouse_down(display: &str, x: i32, y: i32, button: u8) -> Result<(), String> {
    with_conn(display, move |dc| {
        fake_motion(dc, x, y)?;
        sync(dc)?;
        fake_button(dc, button, true)?;
        dc.conn.flush()?;
        sync(dc)
    })
    .await
}

pub async fn mouse_up(display: &str, x: i32, y: i32, button: u8) -> Result<(), String> {
    with_conn(display, move |dc| {
        fake_motion(dc, x, y)?;
        sync(dc)?;
        fake_button(dc, button, false)?;
        dc.conn.flush()?;
        sync(dc)
    })
    .await
}

pub async fn move_mouse(display: &str, x: i32, y: i32) -> Result<(), String> {
    with_conn(display, move |dc| {
        fake_motion(dc, x, y)?;
        sync(dc)
    })
    .await
}

/// Press-drag-release with interpolated intermediate motion, mirroring the
/// session-backend drag (drag-sensitive UIs ignore teleporting pointers).
pub async fn drag(
    display: &str,
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
) -> Result<(), String> {
    with_conn(display, move |dc| {
        fake_motion(dc, start_x, start_y)?;
        sync(dc)?;
        fake_button(dc, 1, true)?;
        dc.conn.flush()?;
        std::thread::sleep(MULTI_CLICK_GAP);
        let mut result = Ok(());
        for i in 1..=DRAG_STEPS {
            let t = i as f64 / DRAG_STEPS as f64;
            let mx = start_x + ((end_x - start_x) as f64 * t).round() as i32;
            let my = start_y + ((end_y - start_y) as f64 * t).round() as i32;
            result = fake_motion(dc, mx, my).and_then(|_| {
                dc.conn.flush()?;
                Ok(())
            });
            if result.is_err() {
                break;
            }
            std::thread::sleep(DRAG_STEP_GAP);
        }
        // Always release the button, even if a motion failed mid-drag.
        let release = fake_button(dc, 1, false).and_then(|_| {
            dc.conn.flush()?;
            sync(dc)
        });
        result.and(release)
    })
    .await
}

/// Scroll `amount` wheel ticks with the given X11 wheel button
/// (4=up, 5=down, 6=left, 7=right).
pub async fn scroll(display: &str, x: i32, y: i32, button: u8, amount: u32) -> Result<(), String> {
    with_conn(display, move |dc| {
        fake_motion(dc, x, y)?;
        sync(dc)?;
        for i in 0..amount.max(1) {
            if i > 0 {
                std::thread::sleep(SCROLL_TICK_GAP);
            }
            fake_button(dc, button, true)?;
            fake_button(dc, button, false)?;
            dc.conn.flush()?;
        }
        sync(dc)
    })
    .await
}

/// Inject a key event sequence (DOM codes + press flag), pacing events like
/// the session backends. A mid-sequence failure releases any key left pressed
/// before returning the error — a stuck modifier corrupts every later action.
pub async fn key_sequence(display: &str, events: Vec<(String, bool)>) -> Result<(), String> {
    with_conn(display, move |dc| {
        let mut outstanding: Vec<u8> = Vec::new();
        let mut result = Ok(());
        for (i, (code, press)) in events.iter().enumerate() {
            let kc = match keycode_for(dc, code) {
                Ok(kc) => kc,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            if i > 0 {
                std::thread::sleep(KEY_EVENT_GAP);
            }
            if let Err(e) = fake_key(dc, kc, *press).and_then(|_| {
                dc.conn.flush()?;
                Ok(())
            }) {
                result = Err(e);
                break;
            }
            if *press {
                outstanding.push(kc);
            } else if let Some(pos) = outstanding.iter().rposition(|&o| o == kc) {
                outstanding.remove(pos);
            }
        }
        if result.is_err() {
            // Best-effort release, most recent first.
            while let Some(kc) = outstanding.pop() {
                let _ = fake_key(dc, kc, false);
            }
            let _ = dc.conn.flush();
        }
        result.and_then(|_| sync(dc))
    })
    .await
}

/// Press all `downs`, hold for `hold_ms`, then inject `ups`. Once anything
/// went down the releases are always attempted (releasing an unpressed key is
/// a harmless no-op).
pub async fn hold_key_sequence(
    display: &str,
    downs: Vec<String>,
    ups: Vec<String>,
    hold_ms: u64,
) -> Result<(), String> {
    with_conn(display, move |dc| {
        let mut errors: Vec<String> = Vec::new();
        let mut pressed_any = false;
        for code in &downs {
            match keycode_for(dc, code).and_then(|kc| {
                fake_key(dc, kc, true)?;
                dc.conn.flush()?;
                Ok(())
            }) {
                Ok(()) => {
                    pressed_any = true;
                    std::thread::sleep(KEY_EVENT_GAP);
                }
                Err(OpError::Conn(e)) | Err(OpError::Other(e)) => {
                    errors.push(format!("key down: {e}"));
                    break;
                }
            }
        }
        if pressed_any && errors.is_empty() {
            std::thread::sleep(Duration::from_millis(hold_ms));
        }
        if pressed_any {
            for code in &ups {
                if let Err(OpError::Conn(e)) | Err(OpError::Other(e)) = keycode_for(dc, code)
                    .and_then(|kc| {
                        fake_key(dc, kc, false)?;
                        dc.conn.flush()?;
                        Ok(())
                    })
                {
                    errors.push(format!("key up: {e}"));
                }
                std::thread::sleep(KEY_EVENT_GAP);
            }
        }
        let _ = sync(dc);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(OpError::Other(errors.join("; ")))
        }
    })
    .await
}

// ── Typing (keysym-based, layout-independent) ───────────────────────────────

fn build_keysym_index(dc: &DisplayConn) -> Result<KeysymIndex, OpError> {
    let count = dc.max_keycode - dc.min_keycode + 1;
    let mapping = dc
        .conn
        .get_keyboard_mapping(dc.min_keycode, count)?
        .reply()?;
    let per = mapping.keysyms_per_keycode as usize;
    let mut map: HashMap<i32, (u8, bool)> = HashMap::new();
    let mut shift_keycode = None;
    let mut scratch_keycode = None;
    for (i, syms) in mapping.keysyms.chunks(per).enumerate() {
        let kc = dc.min_keycode + i as u8;
        let plain = syms.first().copied().unwrap_or(0);
        let shifted = syms.get(1).copied().unwrap_or(0);
        if plain == 0xffe1 && shift_keycode.is_none() {
            shift_keycode = Some(kc);
        }
        if syms.iter().all(|&s| s == 0) && scratch_keycode.is_none() {
            scratch_keycode = Some(kc);
        }
        // First binding wins (lowest keycode), matching XKeysymToKeycode.
        if plain != 0 {
            map.entry(plain as i32).or_insert((kc, false));
        }
        if shifted != 0 && shifted != plain {
            map.entry(shifted as i32).or_insert((kc, true));
        }
    }
    Ok(KeysymIndex {
        map,
        shift_keycode,
        scratch_keycode,
    })
}

/// Type arbitrary text. Characters present in the server's keyboard mapping
/// are pressed directly (with shift where needed); anything else goes through
/// a scratch keycode temporarily remapped to the character's keysym.
pub async fn type_text(display: &str, text: &str) -> Result<(), String> {
    let text = text.to_string();
    with_conn(display, move |dc| {
        {
            let mut idx = dc.keysym_index.lock().unwrap();
            if idx.is_none() {
                *idx = Some(build_keysym_index(dc)?);
            }
        }
        let (shift_kc, scratch_kc) = {
            let idx = dc.keysym_index.lock().unwrap();
            let idx = idx.as_ref().expect("index built above");
            (idx.shift_keycode, idx.scratch_keycode)
        };

        let mut scratch_in_use = false;
        let mut scratch_current: i32 = 0;
        let result = (|| -> Result<(), OpError> {
            for (i, ch) in text.chars().enumerate() {
                let keysym = char_to_x11_keysym(ch).ok_or_else(|| {
                    OpError::Other(format!("unsupported character: U+{:04X}", ch as u32))
                })?;
                let direct = {
                    let idx = dc.keysym_index.lock().unwrap();
                    idx.as_ref().and_then(|idx| idx.map.get(&keysym).copied())
                };
                let (kc, need_shift) = match direct {
                    Some(hit) => hit,
                    None => {
                        let scratch = scratch_kc.ok_or_else(|| {
                            OpError::Other(format!(
                                "character U+{:04X} is not in the keyboard mapping and the \
                                 server has no free keycode to map it to",
                                ch as u32
                            ))
                        })?;
                        if !scratch_in_use || scratch_current != keysym {
                            dc.conn.change_keyboard_mapping(
                                1,
                                scratch,
                                2,
                                &[keysym as u32, keysym as u32],
                            )?;
                            // The mapping change must be visible to input
                            // processing before the fake key press.
                            sync(dc)?;
                            scratch_in_use = true;
                            scratch_current = keysym;
                        }
                        (scratch, false)
                    }
                };
                if need_shift {
                    let shift = shift_kc.ok_or_else(|| {
                        OpError::Other("no Shift keycode in server keymap".to_string())
                    })?;
                    fake_key(dc, shift, true)?;
                    fake_key(dc, kc, true)?;
                    fake_key(dc, kc, false)?;
                    fake_key(dc, shift, false)?;
                } else {
                    fake_key(dc, kc, true)?;
                    fake_key(dc, kc, false)?;
                }
                dc.conn.flush()?;
                if i % 20 == 19 {
                    std::thread::sleep(KEY_EVENT_GAP);
                } else {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(())
        })();

        if scratch_in_use {
            if let Some(scratch) = scratch_kc {
                // Restore the scratch keycode to unbound.
                let _ = dc.conn.change_keyboard_mapping(1, scratch, 2, &[0, 0]);
                let _ = dc.conn.flush();
            }
        }
        result.and_then(|_| sync(dc))
    })
    .await
}

// ── Clipboard paste ──────────────────────────────────────────────────────────

/// Set the CLIPBOARD selection to `text` (served in-process by the
/// connection's event thread) and press ctrl+v. The previous clipboard is not
/// restored — X11 CU displays are agent-owned. The selection keeps being
/// served after this returns, like a normal clipboard owner, until another
/// client takes the selection or the daemon exits.
pub async fn paste(display: &str, text: &str) -> Result<(), String> {
    if text.len() > PASTE_MAX_BYTES {
        return Err(format!(
            "paste text is {} bytes; the X11 direct-transfer limit is {} — use type or \
             write the content to a file instead",
            text.len(),
            PASTE_MAX_BYTES
        ));
    }
    let text = text.as_bytes().to_vec();
    with_conn(display, move |dc| {
        let _guard = dc.paste_lock.lock().unwrap();
        *dc.serving.lock().unwrap() = Some(ClipboardServing {
            text: Arc::new(text.clone()),
        });
        dc.conn.set_selection_owner(
            dc.selection_window,
            dc.atoms.clipboard,
            x11rb::CURRENT_TIME,
        )?;
        dc.conn.flush()?;
        let owner = dc.conn.get_selection_owner(dc.atoms.clipboard)?.reply()?;
        if owner.owner != dc.selection_window {
            *dc.serving.lock().unwrap() = None;
            return Err(OpError::Other(
                "failed to take ownership of the CLIPBOARD selection".to_string(),
            ));
        }
        // ctrl+v
        let ctrl = keycode_for(dc, "ControlLeft")?;
        let v = keycode_for(dc, "KeyV")?;
        fake_key(dc, ctrl, true)?;
        dc.conn.flush()?;
        std::thread::sleep(KEY_EVENT_GAP);
        fake_key(dc, v, true)?;
        fake_key(dc, v, false)?;
        dc.conn.flush()?;
        std::thread::sleep(KEY_EVENT_GAP);
        fake_key(dc, ctrl, false)?;
        dc.conn.flush()?;
        sync(dc)?;
        // Give the target app time to request and receive the selection —
        // the event thread does the actual serving.
        std::thread::sleep(PASTE_TRANSFER_WINDOW);
        Ok(())
    })
    .await
}

/// Per-connection event loop: the single consumer of the X event queue.
/// Serves clipboard SelectionRequests and drops serving on SelectionClear.
/// Exits when the connection errors out (server gone; the cache entry gets
/// invalidated by the next operation's failed request).
fn event_loop(dc: Arc<DisplayConn>) {
    loop {
        let event = match dc.conn.wait_for_event() {
            Ok(ev) => ev,
            Err(_) => return,
        };
        match event {
            Event::SelectionRequest(req) => {
                let text = dc
                    .serving
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| Arc::clone(&s.text));
                let property = match text {
                    Some(_) if req.target == dc.atoms.targets => {
                        let supported = [
                            dc.atoms.targets,
                            dc.atoms.utf8_string,
                            dc.atoms.text_plain_utf8,
                            xproto::AtomEnum::STRING.into(),
                        ];
                        let ok = dc
                            .conn
                            .change_property32(
                                xproto::PropMode::REPLACE,
                                req.requestor,
                                req.property,
                                xproto::AtomEnum::ATOM,
                                &supported,
                            )
                            .is_ok();
                        if ok {
                            req.property
                        } else {
                            x11rb::NONE
                        }
                    }
                    Some(text)
                        if req.target == dc.atoms.utf8_string
                            || req.target == dc.atoms.text_plain_utf8
                            || req.target == xproto::Atom::from(xproto::AtomEnum::STRING) =>
                    {
                        let ok = dc
                            .conn
                            .change_property8(
                                xproto::PropMode::REPLACE,
                                req.requestor,
                                req.property,
                                req.target,
                                &text,
                            )
                            .is_ok();
                        if ok {
                            req.property
                        } else {
                            x11rb::NONE
                        }
                    }
                    _ => x11rb::NONE,
                };
                let notify = xproto::SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property,
                };
                let _ =
                    dc.conn
                        .send_event(false, req.requestor, xproto::EventMask::NO_EVENT, notify);
                let _ = dc.conn.flush();
            }
            Event::SelectionClear(clear)
                if clear.owner == dc.selection_window && clear.selection == dc.atoms.clipboard =>
            {
                // Another client took the clipboard; stop serving.
                *dc.serving.lock().unwrap() = None;
            }
            _ => {}
        }
    }
}

// ── Screenshot ───────────────────────────────────────────────────────────────

/// Capture the root window as PNG bytes, in-process (no `import` fork, no
/// disk round-trip). Queries live geometry so RandR resizes are respected.
pub async fn screenshot_png(display: &str) -> Result<Vec<u8>, String> {
    with_conn(display, move |dc| {
        let geo = dc.conn.get_geometry(dc.root)?.reply()?;
        let (w, h) = (geo.width, geo.height);
        if w == 0 || h == 0 {
            return Err(OpError::Other(format!("root window is {w}x{h}")));
        }
        let image = dc
            .conn
            .get_image(
                xproto::ImageFormat::Z_PIXMAP,
                dc.root,
                0,
                0,
                w,
                h,
                !0, // all planes
            )?
            .reply()?;
        let data = image.data;
        let (w, h) = (w as usize, h as usize);
        let bpp = if h > 0 && w > 0 {
            data.len() / (w * h)
        } else {
            0
        };
        // ZPixmap depth-24/32 on little-endian servers is BGR(X) byte order —
        // every platform intendant targets.
        let mut rgb = Vec::with_capacity(w * h * 3);
        match bpp {
            4 => {
                for px in data.chunks_exact(4) {
                    rgb.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            3 => {
                for px in data.chunks_exact(3) {
                    rgb.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            _ => {
                return Err(OpError::Other(format!(
                    "unsupported pixel layout: {} bytes for {w}x{h}",
                    data.len()
                )))
            }
        }
        let img = image::RgbImage::from_raw(w as u32, h as u32, rgb)
            .ok_or_else(|| OpError::Other("assemble RGB image".to_string()))?;
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| OpError::Other(format!("encode PNG: {e}")))?;
        Ok(buf.into_inner())
    })
    .await
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paste_size_limit_is_enforced() {
        // The limit check fires before any X connection is attempted, so this
        // is safe on hosts with no X server.
        let text = "x".repeat(PASTE_MAX_BYTES + 1);
        let err = paste(":0", &text).await.unwrap_err();
        assert!(err.contains("direct-transfer limit"), "{err}");
    }

    /// Live test — needs a reachable X server (DISPLAY or :0). Run on the
    /// Linux test boxes with `cargo test --bin intendant x11_input -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_screenshot_and_click() {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let png = screenshot_png(&display).await.expect("screenshot");
        assert!(png.len() > 1000);
        move_mouse(&display, 10, 10).await.expect("move");
        key_sequence(
            &display,
            vec![("KeyA".to_string(), true), ("KeyA".to_string(), false)],
        )
        .await
        .expect("key");
    }
}
