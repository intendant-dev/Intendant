// ── ui-v2 voice: oversight mic button + in-call popover panel ──────────
// User-approved design (2026-07-07): 5 states, transcript stays in the
// Activity log, camera off by default (unchanged v1 behavior).
//
// Mechanism: the WASM presence callbacks hold direct references to the
// v1 overlay's elements (#micBtn #videoBtn #makeActiveBtn #voiceStatus
// #videoPreviewWrap) with no null guards — so under ui-v2 those exact
// nodes are RE-PARENTED into the panel (ids, listeners, and WASM refs
// survive; the v1 overlay shell stays in the DOM, hidden, because
// 22-voice-bootstrap.html asserts it exists). v1 without the flag is
// byte-identical. No presence-web changes.

function ui2VoiceBuildPanel() {
  const bar = document.getElementById('ui2-oversight');
  if (!bar || document.getElementById('ui2-mic-btn')) return;

  const mic = document.createElement('button');
  mic.type = 'button';
  mic.id = 'ui2-mic-btn';
  mic.className = 'ui2-micbtn';
  mic.title = 'Voice';
  mic.dataset.state = 'idle';
  mic.setAttribute('aria-haspopup', 'dialog');
  mic.setAttribute('aria-expanded', 'false');
  mic.innerHTML = `<span class="ui2-micbtn-icon">${ui2Icon('mic', 16)}</span><span class="ui2-micbtn-dot"></span>`;
  const search = document.getElementById('ui2-search-btn');
  bar.insertBefore(mic, search);

  const panel = document.createElement('div');
  panel.id = 'ui2-voice-panel';
  panel.hidden = true;
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Voice');
  panel.innerHTML = `
    <div class="ui2-vp-head">
      <span class="ui2-vp-dot"></span>
      <span class="ui2-vp-title" id="ui2-vp-title">Voice</span>
      <span class="ui2-vp-sub" id="ui2-vp-sub"></span>
      <button type="button" class="ui2-vp-close" id="ui2-vp-close" title="Close">${ui2Icon('close', 13)}</button>
    </div>
    <div class="ui2-vp-body">
      <div class="ui2-vp-status-slot" id="ui2-vp-status-slot"></div>
      <div class="ui2-vp-cam-slot" id="ui2-vp-cam-slot"></div>
      <div class="ui2-vp-controls" id="ui2-vp-controls"></div>
      <div class="ui2-vp-note" id="ui2-vp-note">Live transcript appears in the Activity log.</div>
    </div>`;
  document.body.appendChild(panel);

  // Re-parent the live v1 controls (ids/listeners/WASM refs intact).
  const controls = panel.querySelector('#ui2-vp-controls');
  for (const id of ['micBtn', 'videoBtn', 'makeActiveBtn']) {
    const el = document.getElementById(id);
    if (el) controls.appendChild(el);
  }
  const status = document.getElementById('voiceStatus');
  if (status) panel.querySelector('#ui2-vp-status-slot').appendChild(status);
  const cam = document.getElementById('videoPreviewWrap');
  if (cam) panel.querySelector('#ui2-vp-cam-slot').appendChild(cam);

  // Swap the v1 emoji glyphs for line icons + labels (same grid/stroke as
  // ui2Icon; safe: 36-voice-wasm-init only ever toggles these buttons'
  // classList, never their content — #makeActiveBtn is text-rewritten by
  // v1 ("Requesting active...") so it stays text-only).
  const camSvg = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" style="flex-shrink:0;display:block"><rect x="3" y="6.5" width="13.5" height="11" rx="2.4"/><path d="M16.5 10.6 21 8v8l-4.5-2.6Z"/></svg>';
  const decorate = (btn, iconHtml, label) => {
    if (!btn) return;
    btn.textContent = '';
    const ic = document.createElement('span');
    ic.className = 'ui2-vp-btn-icon';
    ic.innerHTML = iconHtml;
    const tx = document.createElement('span');
    tx.className = 'ui2-vp-btn-label';
    tx.textContent = label;
    btn.append(ic, tx);
  };
  decorate(document.getElementById('micBtn'), ui2Icon('mic', 14), 'Mic');
  decorate(document.getElementById('videoBtn'), camSvg, 'Camera');

  const toggle = (open) => {
    const show = open === undefined ? panel.hidden : !!open;
    panel.hidden = !show;
    mic.setAttribute('aria-expanded', String(show));
  };
  mic.addEventListener('click', () => toggle());
  panel.querySelector('#ui2-vp-close').addEventListener('click', () => toggle(false));
  // Layered-Escape contract: the ⌘K palette's capture handler runs first
  // (earlier fragment) and marks its Escape consumed via preventDefault —
  // honoring that keeps one keypress from closing both layers; we mark
  // ours the same way so the v1 Escape cascade below us stays quiet.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !panel.hidden && !e.defaultPrevented) {
      e.preventDefault(); e.stopPropagation(); toggle(false);
    }
  }, true);
  document.addEventListener('mousedown', (e) => {
    if (!panel.hidden && !panel.contains(e.target) && e.target !== mic && !mic.contains(e.target)) toggle(false);
  });

  // State machine mirrors: #sb-voice (ok/err + provider label) and
  // #sb-active-badge (Active/Passive) stay v1-truth; we re-present.
  const applyState = () => {
    const sbVoice = document.getElementById('sb-voice');
    const sbLabel = document.getElementById('sb-voice-label');
    const badge = document.getElementById('sb-active-badge');
    const cls = (sbVoice && sbVoice.className) || '';
    const label = ((sbLabel && sbLabel.textContent) || '').trim();
    const badgeText = ((badge && badge.textContent) || '').trim().toLowerCase();
    let state = 'idle';
    if (/err/.test(cls)) state = 'error';
    else if (/ok/.test(cls)) state = 'live';
    else if (badgeText.includes('passive')) state = 'passive';
    mic.dataset.state = state;
    panel.dataset.state = state;
    const title = panel.querySelector('#ui2-vp-title');
    title.textContent = state === 'live' ? 'Voice · live'
      : state === 'error' ? 'Voice · error'
      : state === 'passive' ? 'Voice · passive' : 'Voice';
    panel.querySelector('#ui2-vp-sub').textContent = label;
  };
  for (const id of ['sb-voice', 'sb-voice-label', 'sb-active-badge']) {
    const el = document.getElementById(id);
    if (el) new MutationObserver(applyState).observe(el, { attributes: true, childList: true, characterData: true, subtree: true });
  }
  applyState();
}

if (document.readyState === 'complete') ui2VoiceBuildPanel();
else document.addEventListener('DOMContentLoaded', ui2VoiceBuildPanel, { once: true });
