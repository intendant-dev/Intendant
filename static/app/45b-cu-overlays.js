// ── CU action visualization (Live tab) ─────────────────────────────────
// Renders what the agent DOES on a display from the daemon's `cu_action`
// wire lane (one ephemeral event per successfully executed computer-use
// action; see computer_use::CuActionObserver): the agent cursor + verb
// pill, click ripples, keypress chips, the screenshot flash, the
// per-display action feed rows (via window.noteCuDisplayActivity in
// 45-displays-webrtc.js), and the session-attributed rail approval card.
//
// Honesty invariants:
// - Everything here renders daemon-reported facts only. No clicks, typed
//   text, roles, holders, or approvals are ever invented client-side.
// - The concept's target-highlight box + role tag are deliberately NOT
//   implemented: no element/role data exists on the wire today (future
//   AX integration).
// - Approval attribution: the amber rail card renders only when the
//   pending approval's session was REPORTED (via cu_action session_id)
//   as the session driving the selected display. No guessing.
//
// Stage guardrails (ui2-live.css header): overlays are absolutely
// positioned, pointer-events:none, aria-hidden children of
// .display-canvas; geometry comes from surfaceContentBox()
// (getBoundingClientRect + intrinsic dims, pure letterbox); nothing here
// touches the video element's dimensions or transforms.

const CU_OVERLAY_IDLE_FADE_MS = 6000;   // cursor fades out after this much quiet
const CU_OVERLAY_RIPPLE_MS = 560;       // concept-exact click ripple duration
const CU_OVERLAY_FLASH_MS = 400;        // concept-exact screenshot flash duration
const CU_OVERLAY_KEYS_HOLD_MS = 2600;   // keypress chips linger before clearing
const CU_OVERLAY_LERP = 0.16;           // concept-exact cursor ease factor
const CU_OVERLAY_DIMMED_OPACITY = 0.26; // cursor opacity while YOU hold input

const cuReducedMotion = typeof window.matchMedia === 'function'
  ? window.matchMedia('(prefers-reduced-motion: reduce)')
  : { matches: false };

// displayId → { sessionId, at } — the last session the daemon reported as
// driving that display. Fed exclusively by cu_action events; consumed by
// the rail approval card. Entries retire with their display slot.
const cuDisplaySessionAttribution = new Map();

// ── wire kind → presentation vocabularies ──────────────────────────────

const CU_CLICK_KINDS = new Set([
  'left_click', 'right_click', 'middle_click',
  'double_click', 'triple_click', 'mouse_down', 'mouse_up',
]);
const CU_TYPE_KINDS = new Set(['type', 'paste', 'key', 'hold_key']);
const CU_LOOK_KINDS = new Set(['screenshot', 'zoom']);

// Feed dot color class (ui2-live.css kind-* rules): sky = look, iris =
// click, violet = type, neutral = scroll/move, amber = waiting.
function cuFeedKindFor(kind) {
  if (CU_LOOK_KINDS.has(kind)) return 'look';
  if (CU_CLICK_KINDS.has(kind)) return 'click';
  if (CU_TYPE_KINDS.has(kind)) return 'type';
  if (kind === 'wait') return 'attention';
  return 'neutral'; // scroll, move, drag, unknown future kinds
}

// Cursor verb pill (concept vocabulary: Look/Move/Click/Type/Scroll/Waiting).
function cuVerbFor(kind) {
  if (CU_LOOK_KINDS.has(kind)) return 'Look';
  if (CU_CLICK_KINDS.has(kind)) return 'Click';
  if (CU_TYPE_KINDS.has(kind)) return 'Type';
  if (kind === 'scroll') return 'Scroll';
  if (kind === 'wait') return 'Waiting';
  return 'Move'; // move, drag, unknown
}

// Pull the quoted text back out of a raw call string (`type("abc")`).
function cuRawQuotedText(raw) {
  const m = /^[a-z_]+\("([\s\S]*)"\)$/.exec(String(raw || ''));
  return m ? m[1] : '';
}

// Pull the bare argument list out of a raw call string (`key(ctrl+c)`).
function cuRawArgs(raw) {
  const m = /^[a-z_]+\(([^)]*)\)$/.exec(String(raw || ''));
  return m ? m[1] : '';
}

// Percent-encoded typed text reads better decoded in the friendly line
// (`%20name%3D` → " name="). Presentation only — the feed row's mono
// detail keeps the literal call (full text behind its disclosure), so
// nothing daemon-reported is lost or invented.
function cuDecodedPreview(text, max) {
  let t = text;
  if (/%[0-9A-Fa-f]{2}/.test(t)) {
    try {
      t = decodeURIComponent(t.replace(/%(?![0-9A-Fa-f]{2})/g, '%25'));
    } catch (_) { /* not valid percent-encoding — keep the literal text */ }
  }
  return t.length > max ? t.slice(0, max) + '…' : t;
}

// Friendly first line for the feed. Generic on purpose: the wire carries
// coordinates and raw calls, not element semantics, so the sentence never
// pretends to know WHAT was clicked.
function cuFriendlyFor(d) {
  const hasPoint = Number.isFinite(Number(d.x)) && Number.isFinite(Number(d.y));
  const at = hasPoint ? ` at (${d.x}, ${d.y})` : '';
  switch (d.kind) {
    case 'screenshot': return 'Looked at the screen';
    case 'zoom': return 'Looked closer at a region';
    case 'left_click': return 'Clicked' + at;
    case 'right_click': return 'Right-clicked' + at;
    case 'middle_click': return 'Middle-clicked' + at;
    case 'double_click': return 'Double-clicked' + at;
    case 'triple_click': return 'Triple-clicked' + at;
    case 'mouse_down': return 'Pressed the mouse button' + at;
    case 'mouse_up': return 'Released the mouse button' + at;
    case 'move': return 'Moved the pointer' + at;
    case 'drag': return hasPoint ? `Dragged to (${d.x}, ${d.y})` : 'Dragged';
    case 'scroll': {
      const dir = (cuRawArgs(d.raw).split(',')[0] || '').trim();
      return dir ? `Scrolled ${dir}` : 'Scrolled';
    }
    case 'type': {
      const text = cuRawQuotedText(d.raw);
      if (!text) return 'Typed text';
      return `Typed “${cuDecodedPreview(text, 32)}”`;
    }
    case 'paste': return 'Pasted text';
    case 'key': {
      const key = cuRawArgs(d.raw);
      return key ? `Pressed ${key}` : 'Pressed a key';
    }
    case 'hold_key': {
      const key = (cuRawArgs(d.raw).split(',')[0] || '').trim();
      return key ? `Held ${key}` : 'Held a key';
    }
    case 'wait': {
      const ms = Number((/^wait\((\d+)ms\)$/.exec(String(d.raw || '')) || [])[1]);
      if (Number.isFinite(ms) && ms > 0) {
        return ms >= 1000 ? `Waited ${(ms / 1000).toFixed(ms % 1000 ? 1 : 0)}s` : `Waited ${ms}ms`;
      }
      return 'Waited';
    }
    default: return `Performed ${String(d.kind || 'action')}`;
  }
}

// ── per-slot overlay engine ─────────────────────────────────────────────

const cuOverlayEngines = new Map(); // displayId → CuOverlayEngine

class CuOverlayEngine {
  constructor(displayId, slot) {
    this.displayId = Number(displayId);
    this.slot = slot;
    this.raf = 0;
    this.lastFrameAt = 0;
    this.lastActivityAt = 0;
    this.cursorShown = false;
    this.cursorOpacity = 0;
    this.cx = 0; this.cy = 0;   // current cursor position (stage px)
    this.tx = 0; this.ty = 0;   // target cursor position
    this.placed = false;        // cursor has a real position yet
    this.ripple = null;         // { t0, x, y }
    this.flashT0 = null;
    this.keysTimer = 0;

    const layer = (cls, zIndex) => {
      const el = document.createElement('div');
      el.className = cls;
      el.setAttribute('aria-hidden', 'true');
      if (zIndex) el.style.zIndex = String(zIndex);
      slot.canvasEl.appendChild(el);
      return el;
    };
    // z-order mirrors the concept: flash 9 < ripple 16 < keys 18 < cursor 20.
    this.flashEl = layer('cu-overlay-flash', 9);
    this.rippleEl = layer('cu-overlay-ripple', 16);
    this.keysEl = layer('cu-overlay-keys', 18);
    this.cursorEl = layer('cu-overlay-cursor', 20);
    // Presence halo (under the arrow, centered on the hotspot) +
    // concept-exact white arrow (24×26) + iris verb pill. The halo keeps
    // the agent pointer findable on busy content; it rides this element's
    // ease/idle-fade and is CSS-hidden while YOU hold input (the
    // `cu-user-driving` class below) so it never reads as a second live
    // cursor next to your real one.
    this.cursorEl.innerHTML =
      '<span class="cu-cursor-halo"></span>' +
      '<svg width="24" height="26" viewBox="0 0 24 26">' +
      '<path d="M2 2 L2 20.5 L6.6 16.2 L9.9 23.4 L13 22 L9.7 15 L16 14.6 Z" ' +
      'fill="#ffffff" stroke="rgba(10,12,20,.55)" stroke-width="1.2" ' +
      'stroke-linejoin="round"></path></svg>' +
      '<span class="cu-cursor-pill"></span>';
    this.pillEl = this.cursorEl.querySelector('.cu-cursor-pill');
  }

  destroy() {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    if (this.keysTimer) clearTimeout(this.keysTimer);
    for (const el of [this.flashEl, this.rippleEl, this.keysEl, this.cursorEl]) {
      el?.remove();
    }
  }

  // Map a display-space point (against the event's reference resolution,
  // falling back to the stream's intrinsic size) into stage-local pixels.
  stagePoint(x, y, refW, refH) {
    const slot = this.slot;
    if (!slot || !slot.canvasEl || !slot.videoEl) return null;
    const box = surfaceContentBox(slot.canvasEl, slot.videoEl, slot.width, slot.height);
    if (!box) return null;
    const rw = refW > 0 ? refW : (Number(slot.videoEl.videoWidth) || Number(slot.width) || 0);
    const rh = refH > 0 ? refH : (Number(slot.videoEl.videoHeight) || Number(slot.height) || 0);
    if (!(rw > 0) || !(rh > 0)) return null;
    const fx = Math.max(0, Math.min(1, x / rw));
    const fy = Math.max(0, Math.min(1, y / rh));
    return { x: box.x + fx * box.w, y: box.y + fy * box.h };
  }

  handle(d) {
    const now = performance.now();
    this.lastActivityAt = now;
    this.cursorShown = true;
    if (this.pillEl) {
      const verb = cuVerbFor(d.kind);
      if (this.pillEl.textContent !== verb) this.pillEl.textContent = verb;
    }

    const hasPoint = Number.isFinite(Number(d.x)) && Number.isFinite(Number(d.y));
    if (hasPoint) {
      const p = this.stagePoint(Number(d.x), Number(d.y), Number(d.ref_w) || 0, Number(d.ref_h) || 0);
      if (p) {
        this.tx = p.x; this.ty = p.y;
        if (!this.placed || cuReducedMotion.matches) {
          // First placement (or reduced motion): snap, don't fly across.
          this.cx = p.x; this.cy = p.y;
          this.placed = true;
        }
        if (CU_CLICK_KINDS.has(d.kind) && d.kind !== 'mouse_up' && !cuReducedMotion.matches) {
          this.ripple = { t0: now, x: p.x, y: p.y };
        }
      }
    } else if (!this.placed) {
      // Coordinate-free first event: fade in at the stage centre (concept
      // reset behavior) rather than the top-left corner.
      const rect = this.slot.canvasEl.getBoundingClientRect();
      if (rect.width > 0) {
        this.cx = this.tx = rect.width / 2;
        this.cy = this.ty = rect.height * 0.42;
        this.placed = true;
      }
    }

    if (CU_LOOK_KINDS.has(d.kind) && !cuReducedMotion.matches) {
      this.flashT0 = now;
    }
    if (CU_TYPE_KINDS.has(d.kind)) {
      this.showKeyChips(d);
    }
    this.ensureRunning();
  }

  showKeyChips(d) {
    const chips = [];
    if (d.kind === 'type' || d.kind === 'paste') {
      const text = cuRawQuotedText(d.raw);
      // Last ~9 typed characters, space rendered as · (concept grammar).
      for (const ch of Array.from(text).slice(-9)) {
        chips.push(ch === ' ' ? '·' : ch);
      }
      if (!chips.length) return; // nothing honest to show (empty/truncated-away)
    } else {
      const key = (d.kind === 'hold_key')
        ? (cuRawArgs(d.raw).split(',')[0] || '').trim()
        : cuRawArgs(d.raw);
      if (!key) return;
      chips.push(key);
    }
    this.keysEl.replaceChildren(...chips.map(label => {
      const chip = document.createElement('span');
      chip.className = 'cu-key-chip';
      chip.textContent = label;
      return chip;
    }));
    if (this.keysTimer) clearTimeout(this.keysTimer);
    this.keysTimer = window.setTimeout(() => {
      this.keysTimer = 0;
      this.keysEl.replaceChildren();
    }, CU_OVERLAY_KEYS_HOLD_MS);
  }

  ensureRunning() {
    if (this.raf) return;
    this.lastFrameAt = performance.now();
    const step = () => { this.raf = 0; this.tick(); };
    this.raf = requestAnimationFrame(step);
  }

  tick() {
    const slot = displaySlots.get(this.displayId);
    if (slot !== this.slot || !this.slot.canvasEl.isConnected) {
      // Slot replaced or removed under us — retire the engine.
      cuOverlayEngines.delete(this.displayId);
      this.destroy();
      return;
    }
    const now = performance.now();

    // Cursor ease + opacity. The cursor dims while the DASHBOARD USER
    // holds input authority (their pointer drives the display, not the
    // agent's), and fades out entirely after idle.
    const idle = now - this.lastActivityAt > CU_OVERLAY_IDLE_FADE_MS;
    const userDriving = this.slot.authorityState === 'you';
    // classList.toggle with a force flag is a no-op when already in the
    // desired state, so this is safe at animation-frame cadence.
    this.cursorEl.classList.toggle('cu-user-driving', userDriving);
    const targetOpacity = !this.cursorShown || idle || !this.placed
      ? 0
      : (userDriving ? CU_OVERLAY_DIMMED_OPACITY : 1);
    if (cuReducedMotion.matches) {
      this.cx = this.tx; this.cy = this.ty;
      this.cursorOpacity = targetOpacity;
    } else {
      this.cx += (this.tx - this.cx) * CU_OVERLAY_LERP;
      this.cy += (this.ty - this.cy) * CU_OVERLAY_LERP;
      this.cursorOpacity += (targetOpacity - this.cursorOpacity) * 0.18;
    }
    this.cursorEl.style.left = this.cx.toFixed(1) + 'px';
    this.cursorEl.style.top = this.cy.toFixed(1) + 'px';
    this.cursorEl.style.opacity = this.cursorOpacity.toFixed(3);

    // Click ripple: 0.4→2.8 scale, 0.9→0 opacity over 560ms (concept).
    if (this.ripple) {
      const age = now - this.ripple.t0;
      if (age < CU_OVERLAY_RIPPLE_MS) {
        const k = age / CU_OVERLAY_RIPPLE_MS;
        this.rippleEl.style.left = this.ripple.x.toFixed(1) + 'px';
        this.rippleEl.style.top = this.ripple.y.toFixed(1) + 'px';
        this.rippleEl.style.transform =
          'translate(-50%,-50%) scale(' + (0.4 + 2.4 * k).toFixed(3) + ')';
        this.rippleEl.style.opacity = (0.9 * (1 - k)).toFixed(3);
      } else {
        this.ripple = null;
        this.rippleEl.style.opacity = '0';
      }
    }

    // Screenshot flash: 0→0.85 over the first quarter, back to 0 (concept).
    if (this.flashT0 != null) {
      const age = now - this.flashT0;
      if (age < CU_OVERLAY_FLASH_MS) {
        const k = age / CU_OVERLAY_FLASH_MS;
        const op = k < 0.25 ? (k / 0.25) * 0.85 : 0.85 * (1 - (k - 0.25) / 0.75);
        this.flashEl.style.opacity = Math.max(0, op).toFixed(3);
      } else {
        this.flashT0 = null;
        this.flashEl.style.opacity = '0';
      }
    }

    const animating =
      this.ripple !== null ||
      this.flashT0 !== null ||
      this.cursorOpacity > 0.005 ||
      Math.abs(this.tx - this.cx) > 0.5 ||
      Math.abs(this.ty - this.cy) > 0.5;
    if (animating) {
      this.raf = requestAnimationFrame(() => { this.raf = 0; this.tick(); });
    } else {
      // Fully settled and faded — park the loop until the next event.
      this.cursorShown = false;
    }
  }
}

function cuEngineFor(displayId) {
  const id = Number(displayId);
  const slot = displaySlots.get(id);
  if (!slot || !slot.canvasEl) return null;
  let engine = cuOverlayEngines.get(id);
  if (engine && engine.slot !== slot) {
    engine.destroy();
    engine = null;
  }
  if (!engine) {
    engine = new CuOverlayEngine(id, slot);
    cuOverlayEngines.set(id, engine);
  }
  return engine;
}

// ── wire entry point (dispatched from 36-voice-wasm-init.js) ────────────

window.handleCuActionEvent = function(d) {
  if (!d || typeof d !== 'object') return;
  const displayId = Number(d.display_id);
  const kind = String(d.kind || '');
  if (!Number.isFinite(displayId) || !kind) return;

  // Display→session attribution (drives the rail approval card).
  const sessionId = typeof d.session_id === 'string' ? d.session_id.trim() : '';
  if (sessionId) {
    cuDisplaySessionAttribution.set(displayId, { sessionId, at: Date.now() });
  }

  // Per-display feed row (two-line action grammar, 45-displays-webrtc.js).
  if (typeof window.noteCuDisplayActivity === 'function') {
    window.noteCuDisplayActivity(displayId, cuFeedKindFor(kind), cuFriendlyFor(d), d.raw || '');
  }

  // Stage overlays for the display's live slot (no slot → feed only).
  const engine = cuEngineFor(displayId);
  if (engine) engine.handle(d);

  scheduleCuApprovalCard();
};

// Attribution retires with its display slot (removeDisplaySlot calls this
// through the live-workspace retire hook chain in 45-displays-webrtc.js).
const cuPreviousRetire = window.retireLiveDisplayWorkspaceSlot;
window.retireLiveDisplayWorkspaceSlot = function(displayId) {
  const id = Number(displayId);
  cuDisplaySessionAttribution.delete(id);
  const engine = cuOverlayEngines.get(id);
  if (engine) {
    cuOverlayEngines.delete(id);
    engine.destroy();
  }
  if (typeof cuPreviousRetire === 'function') cuPreviousRetire(displayId);
  scheduleCuApprovalCard();
};

// ── session-attributed rail approval card ───────────────────────────────
// Renders between Input authority and Display activity when the pending
// approval's session is the daemon-reported driver of the SELECTED
// display. Approve/Deny proxy the main approval panel's own actions
// (window.sendApproval → the session-scoped approve/deny ControlMsg), so
// there is exactly one approval state machine.

let cuApprovalRaf = 0;

function cuApprovalDom() {
  const mount = document.getElementById('ui2-live-approval');
  if (!mount) return null;
  if (!mount.dataset.built) {
    mount.dataset.built = '1';
    mount.innerHTML =
      '<div class="cu-approval-card">' +
        '<div class="cu-approval-head">' +
          '<span class="cu-approval-dot" aria-hidden="true"></span>' +
          '<span class="cu-approval-eyebrow">Approval needed</span>' +
        '</div>' +
        '<div class="cu-approval-title">The agent is paused on this display</div>' +
        '<div class="cu-approval-body">The session driving this display asked to run a ' +
          'gated command and is waiting for your decision. The full request is in the ' +
          'approval panel.</div>' +
        '<div class="cu-approval-chips"></div>' +
        '<div class="cu-approval-actions">' +
          '<button type="button" class="cu-approval-approve">Approve &amp; continue</button>' +
          '<button type="button" class="cu-approval-deny">Deny</button>' +
        '</div>' +
      '</div>';
    mount.querySelector('.cu-approval-approve').addEventListener('click', () => {
      if (typeof window.sendApproval === 'function') window.sendApproval('approve');
    });
    mount.querySelector('.cu-approval-deny').addEventListener('click', () => {
      if (typeof window.sendApproval === 'function') window.sendApproval('deny');
    });
  }
  return mount;
}

function renderCuApprovalCard() {
  cuApprovalRaf = 0;
  const mount = cuApprovalDom();
  if (!mount) return;

  const approvalId = typeof pendingApprovalId !== 'undefined' ? pendingApprovalId : null;
  const approvalSession = typeof pendingApprovalSessionId !== 'undefined'
    ? String(pendingApprovalSessionId || '')
    : '';
  const container = document.getElementById('displays-container');
  const selRaw = container ? String(container.dataset.activeDisplayId || '') : '';
  const selectedId = selRaw === '' ? NaN : Number(selRaw);
  const attribution = Number.isFinite(selectedId)
    ? cuDisplaySessionAttribution.get(selectedId)
    : null;

  const show = approvalId !== null &&
    approvalSession !== '' &&
    Boolean(attribution) &&
    attribution.sessionId === approvalSession;
  mount.hidden = !show;
  if (!show) return;

  // Chips mirror the approval panel's shown request (single source of
  // truth for the command text and category).
  const chipsEl = mount.querySelector('.cu-approval-chips');
  const command = (document.getElementById('approval-command')?.textContent || '').trim();
  const categoryEl = document.getElementById('approval-category');
  const category = categoryEl && categoryEl.style.display !== 'none'
    ? (categoryEl.textContent || '').trim()
    : '';
  const chips = [];
  if (command) chips.push(command.length > 64 ? command.slice(0, 64) + '…' : command);
  if (category) chips.push(category);
  chipsEl.replaceChildren(...chips.map(text => {
    const chip = document.createElement('span');
    chip.className = 'cu-approval-chip';
    chip.textContent = text;
    return chip;
  }));

  const sending = typeof approvalSendPending !== 'undefined' && Boolean(approvalSendPending);
  for (const btn of mount.querySelectorAll('button')) btn.disabled = sending;
}

function scheduleCuApprovalCard() {
  if (cuApprovalRaf) return;
  cuApprovalRaf = requestAnimationFrame(renderCuApprovalCard);
}

// Re-render on the approval panel's own lifecycle (show/hide/sending) and
// on stage selection changes — both are DOM-observable without touching
// the approval state machine.
{
  const approvalPanel = document.getElementById('approval-panel');
  if (approvalPanel) {
    new MutationObserver(scheduleCuApprovalCard).observe(approvalPanel, {
      attributes: true,
      attributeFilter: ['class'],
      subtree: true,
      childList: true,
      characterData: true,
    });
  }
  const displaysContainer = document.getElementById('displays-container');
  if (displaysContainer) {
    new MutationObserver(scheduleCuApprovalCard).observe(displaysContainer, {
      attributes: true,
      attributeFilter: ['data-active-display-id'],
    });
  }
}

// Stable browser-QA surface (CDP probes; same convention as qa.liveDisplay).
window.qa = Object.assign(window.qa || {}, {
  cuOverlays() {
    return {
      engines: Array.from(cuOverlayEngines.keys()),
      attribution: Array.from(cuDisplaySessionAttribution.entries()).map(([id, a]) => ({
        displayId: id,
        sessionId: a.sessionId,
        at: a.at,
      })),
      approvalCardVisible: !(document.getElementById('ui2-live-approval')?.hidden ?? true),
    };
  },
});
