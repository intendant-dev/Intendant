// ---- Sealed manifests: refs strip, drift chip, Review & adopt
// (Track AW §2.4/§2.7) ----
// Manifests carrying binding refs grow a refs strip on the inspector's
// effect card: locator, pin prefix, the sealed bytes one expander away
// (served from the content-addressed lane), and an expand-time drift
// chip — the daemon's file_ref_drift judgment extended to manifest
// refs. Drift is INFORMATIONAL: firings keep executing the sealed
// revision whatever happens to the live file (§2.7 / PR-B semantics).
// "Review & adopt" shows sealed vs live and, on one confirm,
// re-proposes the affected manifests with the fresh pin through the
// EXISTING propose lane — new digests, approvals void, landing on the
// ordinary approval (the one-gesture sheet for multi-node). Nothing
// ever auto-adopts; declining leaves the sealed revision binding
// indefinitely.

// Expand-time drift judgments per item id ({at_ms, rows} or
// {inflight:true}): the inspector re-renders on poll ticks, so
// judgments cache briefly instead of re-hashing files every 2s; adopt
// and recheck bust the entry.
const agendaSealDriftCache = new Map();
const AGENDA_SEAL_DRIFT_TTL_MS = 30_000;

function agendaSealDriftRows(itemId) {
  const hit = agendaSealDriftCache.get(itemId);
  if (hit && !hit.inflight && Date.now() - hit.at_ms < AGENDA_SEAL_DRIFT_TTL_MS) {
    return hit.rows;
  }
  if (!hit || (!hit.inflight && Date.now() - hit.at_ms >= AGENDA_SEAL_DRIFT_TTL_MS)) {
    agendaSealDriftFetch(itemId);
  }
  return hit && hit.rows ? hit.rows : null;
}

function agendaSealDriftFetch(itemId) {
  const prev = agendaSealDriftCache.get(itemId);
  agendaSealDriftCache.set(itemId, { ...(prev || {}), inflight: true });
  daemonApi.request('api_agenda_ref_drift', { item_id: itemId })
    .then((res) => {
      const rows = res.ok && res.body && Array.isArray(res.body.binding_refs)
        ? res.body.binding_refs : [];
      agendaSealDriftCache.set(itemId, { at_ms: Date.now(), rows });
    })
    .catch(() => {
      agendaSealDriftCache.set(itemId, { at_ms: Date.now(), rows: null });
    })
    .finally(() => {
      if (typeof agendaInspectorRender === 'function') agendaInspectorRender();
    });
}

function agendaSealLocatorName(locator) {
  const path = String(locator || '').replace(/^file:/, '');
  const parts = path.split('/').filter(Boolean);
  // SKILL.md files are named by their definition directory (spec law).
  const base = parts.pop() || path;
  return base === 'SKILL.md' && parts.length ? `${parts.pop()}/SKILL.md` : base;
}

// The strip: one row per sealed ref — locator, pin chip, drift chip,
// and the sealed view (a <details> the toggle wire fills from
// GET /api/agenda/sealed/{sha256} on first open). Renders on the
// inspector's effect card only; the Automations lens row carries just
// the cheap "sealed ×N" count (list render never hashes).
function agendaSealsStripHtml(item) {
  const st = typeof agendaEffectState === 'function' ? agendaEffectState(item) : null;
  const refs = st && st.manifest && Array.isArray(st.manifest.binding_refs)
    ? st.manifest.binding_refs : [];
  if (!refs.length) return '';
  const driftRows = agendaSealDriftRows(item.id);
  const rows = refs.map((r) => {
    const short = String(r.sha256 || '').slice(0, 12);
    const row = driftRows
      ? driftRows.find((d) => d.locator === r.locator && d.pin === r.sha256)
      : null;
    let drift = '<span class="ag2-seal-drift judging" title="Re-hashing the live file against the sealed pin — judged when the panel opens, never on list render">checking…</span>';
    let adopt = '';
    if (driftRows && !row) drift = '';
    if (row) {
      if (row.status === 'unchanged') {
        drift = '<span class="ag2-seal-drift unchanged" title="The live file still matches the sealed revision">live file unchanged</span>';
      } else if (row.status === 'changed') {
        drift = '<span class="ag2-seal-drift changed" title="Informational — firings keep executing the sealed revision until you adopt the fresh one">live file moved on — sealed revision still serves</span>';
        adopt = `<button type="button" class="ag2-linkbtn ag2-seal-adopt" data-seal-act="adopt"
          data-seal-item="${escapeHtml(item.id)}" data-seal-locator="${escapeHtml(r.locator)}"
          title="Review sealed vs live; one confirm re-proposes the affected manifests on the fresh revision — new digests, approvals void, nothing re-arms until you approve">Review &amp; adopt…</button>`;
      } else {
        drift = '<span class="ag2-seal-drift missing" title="The live file is gone or unreadable — harmless to firings: the sealed copy serves">live file missing — sealed copy serves</span>';
      }
    }
    return `<div class="ag2-seal-row">
      <span class="ag2-seal-loc" title="${escapeHtml(r.locator)}">${escapeHtml(agendaSealLocatorName(r.locator))}</span>
      <button type="button" class="ag2-digest-chip" data-copy-digest="${escapeHtml(r.sha256 || '')}"
        title="${escapeHtml(`sha256 ${r.sha256} — the sealed revision every firing executes — click to copy`)}">sealed ${escapeHtml(short)}…</button>
      ${drift}${adopt}
    </div>
    <details class="ag2-seal-exact" data-seal-sha="${escapeHtml(r.sha256 || '')}">
      <summary>sealed view — the exact bytes firings execute</summary>
      <pre class="ag2-seal-pre" data-seal-unloaded="1">Loading the sealed revision…</pre>
    </details>`;
  }).join('');
  return `<div class="ag2-seals">
    <div class="ag2-seals-head">
      <span>Sealed inputs</span>
      <span class="ag2-hint">the approval digest covers these pins — the live file cannot change what runs</span>
    </div>
    ${rows}
  </div>`;
}

// Fill a sealed view on first expand — content-addressed, so the fetch
// happens once per open panel and re-verifies nothing client-side (the
// serving lane already refuses bytes that do not re-hash to the pin).
document.addEventListener('toggle', (event) => {
  const details = event.target;
  if (!details || !details.classList || !details.classList.contains('ag2-seal-exact')) return;
  if (!details.open) return;
  const pre = details.querySelector('pre[data-seal-unloaded]');
  if (!pre) return;
  delete pre.dataset.sealUnloaded;
  daemonApi.request('api_agenda_sealed', { sha256: details.dataset.sealSha })
    .then((res) => {
      pre.textContent = res.ok && res.body && res.body.encoding === 'utf8'
        ? res.body.content
        : `Sealed view unavailable (${(res.body && res.body.error) || `status ${res.status}`})`;
    })
    .catch((e) => { pre.textContent = `Sealed view unavailable (${(e && e.message) || e})`; });
}, true);

// ---- Review & adopt (§2.7) ----

function agendaSealAdoptEnsureSheet() {
  let host = document.getElementById('agenda-adopt-sheet');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'agenda-adopt-sheet';
  host.hidden = true;
  const backdrop = document.createElement('div');
  backdrop.className = 'ags-backdrop';
  backdrop.addEventListener('click', agendaSealAdoptCloseSheet);
  const panel = document.createElement('div');
  panel.className = 'ags-panel agsx-panel ag2-adopt-panel';
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Review and adopt the fresh definition revision');
  host.appendChild(backdrop);
  host.appendChild(panel);
  document.body.appendChild(host);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !host.hidden) agendaSealAdoptCloseSheet();
  });
  return host;
}

function agendaSealAdoptCloseSheet() {
  const host = document.getElementById('agenda-adopt-sheet');
  if (!host) return;
  host.hidden = true;
  host.classList.remove('sheet', 'popover');
}

// Every open item whose manifest pins this locator — a workflow's N
// nodes share one file by construction, so adopting from any node's
// card offers the whole set (§2.7: "the affected manifests").
function agendaSealAdoptTargets(locator) {
  return (agendaItems || []).filter((it) => {
    if (it.status !== 'open') return false;
    const m = (((it.effects || [])[0]) || {}).manifest;
    return m && Array.isArray(m.binding_refs)
      && m.binding_refs.some((r) => r.locator === locator);
  });
}

function agendaSealAdoptOpenSheet(itemId, locator) {
  const cached = agendaSealDriftCache.get(itemId);
  const row = cached && cached.rows
    ? cached.rows.find((d) => d.locator === locator) : null;
  const targets = agendaSealAdoptTargets(locator);
  if (!targets.length) return;
  const pin = (((((targets[0].effects || [])[0]) || {}).manifest || {}).binding_refs || [])
    .find((r) => r.locator === locator);
  const host = agendaSealAdoptEnsureSheet();
  const panel = host.querySelector('.ags-panel');
  panel.textContent = '';

  const head = agendaStartSheetEl('div', 'ags-head');
  head.appendChild(agendaStartSheetEl('span', 'ags-title',
    `Review & adopt: ${agendaSealLocatorName(locator)}`));
  const close = agendaStartSheetEl('button', 'ags-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Keep the sealed revision');
  close.addEventListener('click', agendaSealAdoptCloseSheet);
  head.appendChild(close);
  panel.appendChild(head);
  panel.appendChild(agendaStartSheetEl('div', 'ags-sub',
    'The live file has moved on since this revision was sealed. Firings keep executing the sealed revision until you adopt: adopting re-proposes the affected manifests on the fresh revision — new digests, current approvals become void, and nothing re-arms until you approve again. Declining leaves the sealed revision binding indefinitely.'));

  const cols = agendaStartSheetEl('div', 'ag2-adopt-cols');
  panel.appendChild(cols);
  const column = (label) => {
    const col = agendaStartSheetEl('div', 'ag2-adopt-col');
    col.appendChild(agendaStartSheetEl('div', 'ag2-adopt-col-head', label));
    const pre = agendaStartSheetEl('pre', 'agsx-preview');
    pre.textContent = 'Loading…';
    col.appendChild(pre);
    cols.appendChild(col);
    return pre;
  };
  const effect = (targets[0].effects || [])[0] || {};
  const sealedWhen = effect.approval
    ? `approved ${agendaAbsTime(effect.approval.at_ms)}`
    : `proposed ${agendaAbsTime(effect.proposed_ms || 0)}`;
  const sealedPre = column(`Sealed — sha256 ${String((pin || {}).sha256 || '').slice(0, 12)}… · ${sealedWhen}`);
  const liveWhen = row && row.live_mtime_ms ? ` · modified ${agendaAbsTime(row.live_mtime_ms)}` : '';
  const livePre = column(`Live — sha256 ${row && row.live_sha256 ? `${row.live_sha256.slice(0, 12)}…` : 'unavailable'}${liveWhen}`);

  if (pin) {
    daemonApi.request('api_agenda_sealed', { sha256: pin.sha256 })
      .then((res) => {
        sealedPre.textContent = res.ok && res.body && res.body.encoding === 'utf8'
          ? res.body.content
          : `Sealed view unavailable (${(res.body && res.body.error) || `status ${res.status}`})`;
      })
      .catch((e) => { sealedPre.textContent = `Sealed view unavailable (${(e && e.message) || e})`; });
  } else {
    sealedPre.textContent = 'Sealed pin not found on the manifest.';
  }

  const error = agendaStartSheetEl('div', 'ags-error');
  error.hidden = true;

  // Live text serves only for definition-library files (the catalog
  // reads exactly the paths a stamp resolves — no arbitrary-file read
  // lane is added for this sheet); other refs review by hash + date.
  const liveState = { valid: true, reason: '' };
  const path = String(locator || '').replace(/^file:/, '');
  const paintLive = () => {
    const entry = (agendaDefinitionCatalog || []).find((d) => d.path === path);
    if (!entry) {
      livePre.textContent = row && row.status === 'missing'
        ? 'The live file is gone or unreadable.'
        : 'Live text is served only for definition-library files — review this one at its path; the hashes and dates above still tell the drift story.';
      return;
    }
    livePre.textContent = entry.text;
    if (!entry.valid) {
      liveState.valid = false;
      liveState.reason = entry.reason || 'invalid definition';
      const warn = agendaStartSheetEl('div', 'ags-error',
        `The live revision fails validation: ${liveState.reason} — fix the file before adopting; sealed firings would read a broken definition.`);
      panel.insertBefore(warn, error);
      confirm.disabled = true;
    }
  };

  panel.appendChild(agendaStartSheetEl('div', 'ags-sub',
    `Adopting re-proposes ${targets.length} manifest${targets.length === 1 ? '' : 's'}: ${targets.map((t) => t.title).join(' · ')}`));
  panel.appendChild(error);

  const foot = agendaStartSheetEl('div', 'ags-foot');
  const later = agendaStartSheetEl('button', 'ags-btn', 'Keep the sealed revision');
  later.type = 'button';
  later.addEventListener('click', agendaSealAdoptCloseSheet);
  const confirm = agendaStartSheetEl('button', 'ags-btn ags-start',
    `Adopt — re-propose ${targets.length} on the fresh revision`);
  confirm.type = 'button';
  if (!row || !row.live_sha256) {
    confirm.disabled = true;
    confirm.title = 'No live pin to restate — the live file is missing, unreadable, or past the hash bound.';
  }
  confirm.addEventListener('click', () =>
    agendaSealAdoptConfirm(targets, locator, row ? row.live_sha256 : '', confirm, error));
  foot.appendChild(later);
  foot.appendChild(confirm);
  panel.appendChild(foot);

  paintLive();
  if (agendaDefinitionCatalog === null && typeof agendaFetchDefinitionCatalog === 'function') {
    agendaFetchDefinitionCatalog(() => { if (!host.hidden) paintLive(); });
  }
  agendaPresentStartSheet(host, panel, null);
}

// The one adopt emission: for each affected manifest, the SAME fields
// re-proposed with the drifted ref restated at the fresh pin (other
// refs keep their pins — a second drifted ref refuses by name at
// intake; adopt it from its own row). Rides the existing propose lane
// via the shared op wrapper — the edit surfaces stay clients of
// re-propose, never second writers — and the intake re-verifies the
// restated pin against the daemon's own read, so a file that moved
// again mid-review refuses instead of sealing unreviewed bytes.
async function agendaSealAdoptConfirm(targets, locator, liveSha, button, error) {
  button.disabled = true;
  error.hidden = true;
  const adopted = [];
  try {
    for (const target of targets) {
      const m = ((target.effects || [])[0] || {}).manifest;
      if (!m) continue;
      if (m.interactive) {
        throw new Error(`${target.title}: interactive manifests cannot be re-proposed from here`);
      }
      const params = {
        op: 'propose_effect',
        id: target.id,
        goal: m.goal,
        fire_at_ms: m.fire_at_ms,
        binding_refs: (m.binding_refs || []).map((r) => (r.locator === locator
          ? { locator: r.locator, sha256: liveSha }
          : { locator: r.locator, sha256: r.sha256 })),
      };
      if (m.orchestrate) params.orchestrate = true;
      if (m.recurrence) params.recurrence = m.recurrence;
      if (m.trigger) params.trigger = m.trigger;
      if (m.agent_config) params.agent_config = m.agent_config;
      if (m.project_root) params.project_root = m.project_root;
      const updated = await agendaWorkflowOp(params);
      agendaSealDriftCache.delete(target.id);
      adopted.push({
        title: updated.title,
        digest: ((updated.effects || [])[0] || {}).digest || '',
        item: updated,
      });
    }
    agendaSealAdoptCloseSheet();
    if (adopted.length > 1) {
      // Multi-node: the one-gesture sheet again (§2.7) — its single
      // pinned emitter stays the only approval lane.
      agendaWorkflowOpenApprovalSheet({
        title: agendaSealLocatorName(locator),
        sha256: liveSha,
        nodes: adopted,
        hub: null,
        adopted: true,
      });
    } else if (adopted.length === 1) {
      if (typeof agendaOpenInspector === 'function') agendaOpenInspector(adopted[0].item.id);
      if (typeof showControlToast === 'function') {
        showControlToast('success',
          'Re-proposed on the fresh revision — approve the new digest on the card to re-arm.');
      }
    }
  } catch (e) {
    error.textContent = `${(e && e.message) || e}${adopted.length
      ? ` — ${adopted.length} manifest${adopted.length === 1 ? '' : 's'} already re-proposed keep their new digests (append-only; approve or revise them on their cards)` : ''}`;
    error.hidden = false;
    button.disabled = false;
  }
}

// One click wire for the strip's gestures, document-level like the
// digest-copy wire (the strip renders inside the inspector's innerHTML).
document.addEventListener('click', (event) => {
  const btn = event.target && event.target.closest
    && event.target.closest('[data-seal-act]');
  if (!btn) return;
  if (btn.dataset.sealAct === 'adopt') {
    agendaSealAdoptOpenSheet(btn.dataset.sealItem, btn.dataset.sealLocator);
  }
});
