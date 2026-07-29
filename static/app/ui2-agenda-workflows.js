// ---- Definition stamping + the one-gesture workflow approval
// (Track T → AW) ----
// The automate sheet's explicit Stamp gesture calls the shared wrapper
// below — the daemon's stamp op reads, validates, and SEALS the
// definition, parks the instance graph (an instance HUB with the
// orientation body iff workflow, node items placed under it, relies_on
// links), and proposes one manifest per node. STAMPING parks and
// proposes only; for workflows the approval sheet then previews the
// sealed definition and the stamped graph, and the owner's single
// confirm emits one ordinary per-node approval op — the UI batches,
// the semantics never cascade, and no workflow-level object exists
// anywhere (the emission-shape pin holds the approval lane to exactly
// one emitter called from the owner-confirm handler). The same sheet
// closes the Review-&-adopt loop: a multi-node adopt re-proposes its
// manifests (the seals fragment) and lands back here for the batch
// re-approval.

// One agenda op with the shared error/observe discipline.
async function agendaWorkflowOp(params) {
  const res = await daemonApi.request('api_agenda_op', params);
  if (!res.ok || !res.body || !res.body.item) {
    throw new Error((res.body && res.body.error) || `${params.op} failed (${res.status})`);
  }
  agendaObserveServerMessage({ item: res.body.item });
  return res.body.item;
}

// Stamp one instance through the daemon (the stamp op): one request
// reads, validates, and seals the definition, parks the graph, and
// proposes per node — the response is the whole stamped outcome (hub,
// nodes, per-node digests, the sealed pin). Parks and proposes ONLY —
// approval is the separate explicit act on the surface this feeds. A
// mid-stamp failure leaves ordinary parked items visible on the board
// (append-only history; nothing rolls back silently). Every definition
// kind rides this wrapper; `overrides` carries the kind-gated stamp
// fields the sheet collected (cadence/first-fire/executor — prefills
// into the ordinary manifest intake, refused by name where the kind
// takes none).
async function agendaDefinitionStamp(entry, projectRoot, overrides) {
  const params = { definition: entry.name, ...(overrides || {}) };
  if (projectRoot) params.project_root = projectRoot;
  const res = await daemonApi.request('api_agenda_stamp', params);
  if (!res.ok || !res.body || !res.body.stamp) {
    throw new Error((res.body && res.body.error) || `stamp failed (${res.status})`);
  }
  const stamp = res.body.stamp;
  if (stamp.hub) agendaObserveServerMessage({ item: stamp.hub });
  for (const node of stamp.nodes || []) {
    if (node.item) agendaObserveServerMessage({ item: node.item });
  }
  return stamp;
}

let agendaWorkflowSheetOpen = false;

function agendaWorkflowEnsureSheet() {
  let host = document.getElementById('agenda-workflow-sheet');
  if (host) return host;
  host = document.createElement('div');
  host.id = 'agenda-workflow-sheet';
  host.hidden = true;
  const backdrop = document.createElement('div');
  backdrop.className = 'ags-backdrop';
  backdrop.addEventListener('click', agendaWorkflowCloseSheet);
  const panel = document.createElement('div');
  panel.className = 'ags-panel agsx-panel';
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Approve a stamped workflow');
  host.appendChild(backdrop);
  host.appendChild(panel);
  document.body.appendChild(host);
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !host.hidden) agendaWorkflowCloseSheet();
  });
  return host;
}

function agendaWorkflowCloseSheet() {
  const host = document.getElementById('agenda-workflow-sheet');
  if (!host) return;
  host.hidden = true;
  host.classList.remove('sheet', 'popover');
  agendaWorkflowSheetOpen = false;
}

// The stamped graph + the one gesture, briefing-standard shaped: the
// SEALED definition first (fetched from the content-addressed serving
// lane — exactly the bytes the stamp sealed, rendered once and in
// full), then each node's row (title, executor, digest chip), then the
// committed recommendation. "Later" keeps everything parked with
// pending digests on the ordinary cards — silence arms nothing.
function agendaWorkflowOpenApprovalSheet(stamped) {
  const host = agendaWorkflowEnsureSheet();
  const panel = host.querySelector('.ags-panel');
  panel.textContent = '';
  agendaWorkflowSheetOpen = true;

  const head = agendaStartSheetEl('div', 'ags-head');
  head.appendChild(agendaStartSheetEl('span', 'ags-title',
    `${stamped.adopted ? 'Re-approve' : 'Approve'}: ${stamped.title}`));
  const close = agendaStartSheetEl('button', 'ags-close', '×');
  close.type = 'button';
  close.setAttribute('aria-label', 'Later');
  close.addEventListener('click', agendaWorkflowCloseSheet);
  head.appendChild(close);
  panel.appendChild(head);
  panel.appendChild(agendaStartSheetEl('div', 'ags-sub', stamped.adopted
    ? 'Adopted and re-sealed — nothing re-arms yet. Each node below was re-proposed on the fresh revision (its old approval is void); approving arms them all again.'
    : 'Stamped and sealed — nothing runs yet. Each node below is its own digest-bound manifest pinning the sealed definition; approving arms them all, and the first node fires on approval.'));

  panel.appendChild(agendaStartSheetEl('label', 'ags-label',
    `The sealed definition (sha256 ${String(stamped.sha256 || '').slice(0, 12)}…) — every node pins exactly these bytes`));
  const sealed = agendaStartSheetEl('pre', 'agsx-preview');
  sealed.textContent = 'Loading the sealed definition…';
  panel.appendChild(sealed);
  const sealedFallback = (detail) => {
    sealed.textContent = `Sealed view unavailable${detail ? ` (${detail})` : ''}` +
      ' — each node item carries a display copy of its section.';
  };
  daemonApi.request('api_agenda_sealed', { sha256: stamped.sha256 })
    .then((res) => {
      if (res.ok && res.body && res.body.encoding === 'utf8') {
        sealed.textContent = res.body.content;
      } else {
        sealedFallback((res.body && res.body.error) || `status ${res.status}`);
      }
    })
    .catch((e) => { sealedFallback(String((e && e.message) || e)); });

  for (const node of stamped.nodes) {
    const row = agendaStartSheetEl('div', 'ags-config-row');
    row.appendChild(agendaStartSheetEl('label', 'ags-label', node.title));
    // Executor as text, digest as the shared chip: each row shows the
    // exact revision "Approve all" would bind for that node. The
    // executor pins ride the manifest the digest covers.
    const pins = ((((node.item || {}).effects || [])[0] || {}).manifest || {}).agent_config;
    const executor = pins && pins.agent
      ? [pins.agent, pins.claude_model, pins.claude_effort].filter(Boolean).join(' · ')
      : 'daemon default';
    const hint = agendaStartSheetEl('div', 'ags-hint');
    hint.innerHTML = `${escapeHtml(executor)} · ${agendaDigestChipHtml(node.digest,
      'Approve all binds this node to exactly this manifest revision')}`;
    row.appendChild(hint);
    panel.appendChild(row);
  }

  panel.appendChild(agendaStartSheetEl('div', 'ags-sub',
    `Recommendation: approve all ${stamped.nodes.length} to arm the workflow — each approval is its own ordinary act; nothing cascades. Later keeps everything parked.`));

  const error = agendaStartSheetEl('div', 'ags-error');
  error.hidden = true;
  panel.appendChild(error);

  const foot = agendaStartSheetEl('div', 'ags-foot');
  const later = agendaStartSheetEl('button', 'ags-btn', 'Later');
  later.type = 'button';
  later.addEventListener('click', agendaWorkflowCloseSheet);
  const approve = agendaStartSheetEl('button', 'ags-btn ags-start', `Approve all ${stamped.nodes.length}`);
  approve.type = 'button';
  approve.addEventListener('click', () => agendaWorkflowApproveConfirm(stamped, approve, error));
  foot.appendChild(later);
  foot.appendChild(approve);
  panel.appendChild(foot);

  agendaPresentStartSheet(host, panel, null);
}

// The single approval-emission lane (pinned by
// workflow_surfaces_stamp_through_the_daemon_with_one_emitter):
// called from the owner-confirm handler below and nowhere else,
// iterating exactly the stamped node set.
async function agendaWorkflowEmitApprovals(batch) {
  for (const node of batch.nodes) {
    await agendaWorkflowOp({ op: 'approve_effect', id: node.item.id, digest: node.digest });
  }
}

// The explicit owner confirm — the one gesture (T0 ruling 9).
async function agendaWorkflowApproveConfirm(stamped, button, error) {
  button.disabled = true;
  error.hidden = true;
  try {
    await agendaWorkflowEmitApprovals(stamped);
    agendaWorkflowCloseSheet();
    if (typeof showControlToast === 'function') {
      showControlToast('success', stamped.adopted
        ? `Re-armed on the fresh revision — ${stamped.nodes.length} approvals recorded.`
        : `Workflow armed — ${stamped.nodes.length} approvals recorded; the first node fires now.`);
    }
    const landId = stamped.hub ? stamped.hub.id
      : (stamped.nodes[0] && stamped.nodes[0].item ? stamped.nodes[0].item.id : null);
    if (landId && typeof agendaOpenInspector === 'function') agendaOpenInspector(landId);
  } catch (e) {
    error.textContent = String((e && e.message) || e);
    error.hidden = false;
    button.disabled = false;
  }
}

// The registry-era picker hooks (stamp-on-click workflow/triggered
// buttons) died with the explicit-Stamp gesture: every kind now renders
// in the automate sheet's picker, selection previews the definition,
// and the ONLY stamp fires from the sheet's Stamp button — through the
// shared wrapper above. This fragment keeps the transport and the
// approval machinery; it renders no pickers and stamps nothing on its
// own.
