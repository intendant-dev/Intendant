/// Map DOM KeyboardEvent.code to Linux evdev keycode.
///
/// Phase 1: physical key semantics only -- this maps physical key positions,
/// not character output. Non-US layouts will produce incorrect characters
/// for text entry. This is a known limitation; a future phase will use the
/// `key` field for character-level injection where the platform supports it.

/// Returns the Linux evdev keycode for the given DOM `KeyboardEvent.code` value,
/// or `None` if the code is unrecognised.
pub fn dom_code_to_evdev(code: &str) -> Option<u32> {
    // Keycodes from linux/input-event-codes.h
    Some(match code {
        // Row 0 — Escape + Function keys
        "Escape" => 1,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,

        // Row 1 — Digits
        "Backquote" => 41,
        "Digit1" => 2,
        "Digit2" => 3,
        "Digit3" => 4,
        "Digit4" => 5,
        "Digit5" => 6,
        "Digit6" => 7,
        "Digit7" => 8,
        "Digit8" => 9,
        "Digit9" => 10,
        "Digit0" => 11,
        "Minus" => 12,
        "Equal" => 13,
        "Backspace" => 14,

        // Row 2 — QWERTY
        "Tab" => 15,
        "KeyQ" => 16,
        "KeyW" => 17,
        "KeyE" => 18,
        "KeyR" => 19,
        "KeyT" => 20,
        "KeyY" => 21,
        "KeyU" => 22,
        "KeyI" => 23,
        "KeyO" => 24,
        "KeyP" => 25,
        "BracketLeft" => 26,
        "BracketRight" => 27,
        "Backslash" => 43,

        // Row 3 — ASDF
        "CapsLock" => 58,
        "KeyA" => 30,
        "KeyS" => 31,
        "KeyD" => 32,
        "KeyF" => 33,
        "KeyG" => 34,
        "KeyH" => 35,
        "KeyJ" => 36,
        "KeyK" => 37,
        "KeyL" => 38,
        "Semicolon" => 39,
        "Quote" => 40,
        "Enter" => 28,

        // Row 4 — ZXCV
        "ShiftLeft" => 42,
        "KeyZ" => 44,
        "KeyX" => 45,
        "KeyC" => 46,
        "KeyV" => 47,
        "KeyB" => 48,
        "KeyN" => 49,
        "KeyM" => 50,
        "Comma" => 51,
        "Period" => 52,
        "Slash" => 53,
        "ShiftRight" => 54,

        // Row 5 — Bottom
        "ControlLeft" => 29,
        "MetaLeft" => 125,
        "AltLeft" => 56,
        "Space" => 57,
        "AltRight" => 100,
        "MetaRight" => 126,
        "ControlRight" => 97,

        // Navigation cluster
        "PrintScreen" => 99,
        "ScrollLock" => 70,
        "Pause" => 119,
        "Insert" => 110,
        "Home" => 102,
        "PageUp" => 104,
        "Delete" => 111,
        "End" => 107,
        "PageDown" => 109,

        // Arrow keys
        "ArrowUp" => 103,
        "ArrowLeft" => 105,
        "ArrowDown" => 108,
        "ArrowRight" => 106,

        // Numpad
        "NumLock" => 69,
        "NumpadDivide" => 98,
        "NumpadMultiply" => 55,
        "NumpadSubtract" => 74,
        "Numpad7" => 71,
        "Numpad8" => 72,
        "Numpad9" => 73,
        "NumpadAdd" => 78,
        "Numpad4" => 75,
        "Numpad5" => 76,
        "Numpad6" => 77,
        "Numpad1" => 79,
        "Numpad2" => 80,
        "Numpad3" => 81,
        "NumpadEnter" => 96,
        "Numpad0" => 82,
        "NumpadDecimal" => 83,

        _ => return None,
    })
}

/// Returns the X11 keycode for a DOM `KeyboardEvent.code`, or `None` if the
/// code is unrecognised.
///
/// X11 keycodes on evdev-based servers (Xorg with evdev/libinput, Xvfb with
/// the default "evdev" XKB rules) are the kernel evdev keycode plus 8 — the
/// historic X minimum-keycode offset. This holds for every server intendant
/// targets; callers should still bounds-check against the server's
/// `min_keycode..=max_keycode` from the connection setup.
// Consumers (x11_input, wayland) are Linux-gated; the table stays ungated so
// its unit tests run on every host.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn dom_code_to_x11_keycode(code: &str) -> Option<u8> {
    let evdev = dom_code_to_evdev(code)?;
    u8::try_from(evdev + 8).ok()
}

/// Map a character to the X11 keysym that produces it.
///
/// ASCII and Latin-1 map directly to their codepoint; a handful of control
/// characters map to their editing keysyms (Return, Tab, BackSpace, Escape);
/// everything else uses the standard Unicode keysym rule
/// (`0x01000000 | codepoint`). Shared by the Wayland portal keysym path and
/// the in-process X11 `type_text` path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn char_to_x11_keysym(ch: char) -> Option<i32> {
    match ch {
        '\n' | '\r' => Some(0xff0d),
        '\t' => Some(0xff09),
        '\u{8}' => Some(0xff08),
        '\u{1b}' => Some(0xff1b),
        ' '..='~' => Some(ch as i32),
        '\u{a0}'..='\u{ff}' => Some(ch as i32),
        _ => {
            let code = ch as u32;
            if code <= 0x10ffff {
                Some((0x01000000 | code) as i32)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_keycode_is_evdev_plus_eight() {
        assert_eq!(dom_code_to_x11_keycode("KeyA"), Some(38));
        assert_eq!(dom_code_to_x11_keycode("ControlLeft"), Some(37));
        assert_eq!(dom_code_to_x11_keycode("KeyV"), Some(55));
        assert_eq!(dom_code_to_x11_keycode("Enter"), Some(36));
        assert_eq!(dom_code_to_x11_keycode("BogusKey"), None);
    }

    #[test]
    fn char_keysyms_cover_ascii_and_specials() {
        assert_eq!(char_to_x11_keysym('g'), Some(0x67));
        assert_eq!(char_to_x11_keysym('C'), Some(0x43));
        assert_eq!(char_to_x11_keysym(' '), Some(0x20));
        assert_eq!(char_to_x11_keysym('\n'), Some(0xff0d));
        assert_eq!(char_to_x11_keysym('é'), Some(0xe9));
        assert_eq!(char_to_x11_keysym('€'), Some(0x0100_20ac));
    }

    #[test]
    fn letter_keys() {
        assert_eq!(dom_code_to_evdev("KeyA"), Some(30));
        assert_eq!(dom_code_to_evdev("KeyZ"), Some(44));
        assert_eq!(dom_code_to_evdev("KeyM"), Some(50));
    }

    #[test]
    fn digit_keys() {
        assert_eq!(dom_code_to_evdev("Digit1"), Some(2));
        assert_eq!(dom_code_to_evdev("Digit0"), Some(11));
    }

    #[test]
    fn function_keys() {
        assert_eq!(dom_code_to_evdev("F1"), Some(59));
        assert_eq!(dom_code_to_evdev("F10"), Some(68));
        assert_eq!(dom_code_to_evdev("F11"), Some(87));
        assert_eq!(dom_code_to_evdev("F12"), Some(88));
    }

    #[test]
    fn modifiers() {
        assert_eq!(dom_code_to_evdev("ShiftLeft"), Some(42));
        assert_eq!(dom_code_to_evdev("ShiftRight"), Some(54));
        assert_eq!(dom_code_to_evdev("ControlLeft"), Some(29));
        assert_eq!(dom_code_to_evdev("ControlRight"), Some(97));
        assert_eq!(dom_code_to_evdev("AltLeft"), Some(56));
        assert_eq!(dom_code_to_evdev("AltRight"), Some(100));
        assert_eq!(dom_code_to_evdev("MetaLeft"), Some(125));
        assert_eq!(dom_code_to_evdev("MetaRight"), Some(126));
    }

    #[test]
    fn special_keys() {
        assert_eq!(dom_code_to_evdev("Escape"), Some(1));
        assert_eq!(dom_code_to_evdev("Enter"), Some(28));
        assert_eq!(dom_code_to_evdev("Backspace"), Some(14));
        assert_eq!(dom_code_to_evdev("Tab"), Some(15));
        assert_eq!(dom_code_to_evdev("Space"), Some(57));
        assert_eq!(dom_code_to_evdev("CapsLock"), Some(58));
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(dom_code_to_evdev("ArrowUp"), Some(103));
        assert_eq!(dom_code_to_evdev("ArrowDown"), Some(108));
        assert_eq!(dom_code_to_evdev("ArrowLeft"), Some(105));
        assert_eq!(dom_code_to_evdev("ArrowRight"), Some(106));
        assert_eq!(dom_code_to_evdev("Insert"), Some(110));
        assert_eq!(dom_code_to_evdev("Delete"), Some(111));
        assert_eq!(dom_code_to_evdev("Home"), Some(102));
        assert_eq!(dom_code_to_evdev("End"), Some(107));
        assert_eq!(dom_code_to_evdev("PageUp"), Some(104));
        assert_eq!(dom_code_to_evdev("PageDown"), Some(109));
    }

    #[test]
    fn punctuation_keys() {
        assert_eq!(dom_code_to_evdev("Minus"), Some(12));
        assert_eq!(dom_code_to_evdev("Equal"), Some(13));
        assert_eq!(dom_code_to_evdev("BracketLeft"), Some(26));
        assert_eq!(dom_code_to_evdev("BracketRight"), Some(27));
        assert_eq!(dom_code_to_evdev("Backslash"), Some(43));
        assert_eq!(dom_code_to_evdev("Semicolon"), Some(39));
        assert_eq!(dom_code_to_evdev("Quote"), Some(40));
        assert_eq!(dom_code_to_evdev("Backquote"), Some(41));
        assert_eq!(dom_code_to_evdev("Comma"), Some(51));
        assert_eq!(dom_code_to_evdev("Period"), Some(52));
        assert_eq!(dom_code_to_evdev("Slash"), Some(53));
    }

    #[test]
    fn numpad_keys() {
        assert_eq!(dom_code_to_evdev("NumLock"), Some(69));
        assert_eq!(dom_code_to_evdev("Numpad0"), Some(82));
        assert_eq!(dom_code_to_evdev("Numpad5"), Some(76));
        assert_eq!(dom_code_to_evdev("NumpadEnter"), Some(96));
        assert_eq!(dom_code_to_evdev("NumpadAdd"), Some(78));
        assert_eq!(dom_code_to_evdev("NumpadDecimal"), Some(83));
    }

    #[test]
    fn misc_keys() {
        assert_eq!(dom_code_to_evdev("PrintScreen"), Some(99));
        assert_eq!(dom_code_to_evdev("ScrollLock"), Some(70));
        assert_eq!(dom_code_to_evdev("Pause"), Some(119));
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(dom_code_to_evdev("BogusKey"), None);
        assert_eq!(dom_code_to_evdev(""), None);
    }
}
