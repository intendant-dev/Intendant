// ── Files-transfer state machine ─────────────────────────────────────────
// Every `status` write on a filesTransfers entry flows through this module:
// filesTransferFsmInit() when an entry is created (queue* entry points,
// localStorage restore, server-job merge) and filesTransferTransition() for
// every mutation afterwards. The table below is the contract that used to be
// implicit across ~20 ad-hoc mutation sites in 54-session-lifecycle.js; the
// runner/pump invariants it encodes are load-bearing:
//
//  - Runners settle `status` away from 'queued' before any guard can throw
//    (runFiles*Transfer marks 'running' as its first statement):
//    pumpFilesTransfers re-picks 'queued' entries, so an entry left 'queued'
//    after its runner returned would be re-picked forever — a microtask spin
//    that freezes the tab and allocates without bound.
//  - The pump backstop force-fails an entry whose runner exited without
//    settling it, and that transition MUST reject the completion promise —
//    unless the entry's queueEpoch moved, which marks a legitimate re-queue
//    (Resume/Retry) racing the runner's teardown.
//  - Terminal statuses settle `completion`: entering 'completed' resolves
//    it, entering 'failed'/'cancelled' rejects it. The transition performs
//    the settle, so a terminal entry can never strand an awaiting caller.
//    (Settling an already-settled promise is a no-op, so late duplicates —
//    retry-after-completed, server re-merges — are harmless.)
//  - Entering 'queued' re-arms the attempt: queueEpoch bumps, the teardown
//    flags clear, and a fresh completion promise is minted for the new run.
//
// Statuses: queued | running | paused | failed | cancelled | completed,
// plus 'ready' on server-side transfer jobs merged from api_transfer_jobs.
// Actors: 'user' (transfer-pane buttons + the queue* entry points),
// 'runner' (runFilesDownloadTransfer / runFilesUploadTransfer and their
// settle paths), 'pump' (pumpFilesTransfers' forward-progress backstop),
// 'restore' (restoreFilesTransferState), 'server' (filesTransferMergeServerJob).

const FILES_TRANSFER_STATUSES = new Set([
  'queued', 'running', 'paused', 'failed', 'cancelled', 'completed', 'ready',
]);

// Settled history: pruning, Clear-history, and the persistence filter treat
// these as finished; the FSM settles the completion promise on entry.
const FILES_TRANSFER_TERMINAL_STATUSES = new Set(['completed', 'cancelled', 'failed']);

// from-status → to-status → actors allowed to perform it. The 'server'
// actor is not listed here: filesTransferMergeServerJob may overwrite any
// row whose local status is not actively owned by this tab (see
// filesTransferTransitionAllowed below).
const FILES_TRANSFER_TRANSITIONS = {
  queued: {
    running: ['runner'],   // pump picked the entry; runner marks it before any guard can throw
    paused: ['user'],      // Pause on a not-yet-started entry parks it in place
    cancelled: ['user'],   // Cancel before the pump picks the entry up
    failed: ['pump'],      // backstop: runner exited with the entry unsettled (epoch unchanged)
  },
  running: {
    completed: ['runner'],
    paused: ['runner'],    // pauseRequested teardown / AbortError
    cancelled: ['runner'], // cancelRequested teardown
    failed: ['runner', 'pump'],
  },
  paused: {
    queued: ['user'],      // Resume
    cancelled: ['user'],
  },
  failed: {
    queued: ['user'],      // Resume (downloads with partial progress) or Retry
  },
  cancelled: {
    queued: ['user'],      // Retry
  },
  completed: {
    // Documented exceptions — deliberate quirks, not bugs:
    queued: ['user'],      // Retry re-runs a finished transfer from scratch
    failed: ['user'],      // Save after reload with the cached download chunks evicted
  },
  ready: {},               // server-owned; only the server merge moves it
};

// Statuses an entry may be created in, per creating actor. The queue*
// entry points always mint 'queued'; restoreFilesTransferState only sees
// what filesTransferPersistable writes (running/queued normalize to
// 'paused'; cancelled rows and completed uploads are dropped — a restored
// 'completed' is always a download); server merges mirror the job's own
// status (filesTransferStatusFromJob normalizes unknowns to 'queued').
const FILES_TRANSFER_INITIAL_STATUSES = {
  user: ['queued'],
  restore: ['paused', 'failed', 'completed', 'ready'],
  server: ['queued', 'running', 'paused', 'failed', 'cancelled', 'completed', 'ready'],
};

const FILES_TRANSFER_FSM_TAG = '[transfer-fsm]';
let filesTransferFsmIllegalCount = 0;
const filesTransferFsmRecentReports = [];

// Diagnostics only — the offending transition is still applied afterwards
// (the UI must keep working; the table polices intent, it never gates
// behavior). NEVER throw from here.
function filesTransferFsmReport(message, transfer, detail = {}) {
  filesTransferFsmIllegalCount += 1;
  const report = {
    message,
    id: transfer?.id || '',
    kind: transfer?.kind || '',
    ...detail,
  };
  filesTransferFsmRecentReports.push(report);
  if (filesTransferFsmRecentReports.length > 20) filesTransferFsmRecentReports.shift();
  console.error(FILES_TRANSFER_FSM_TAG, message, report);
}

function filesTransferTransitionAllowed(from, to, actor) {
  if (actor === 'server') {
    // Mirror of the filesTransferMergeServerJob guard: a row whose local
    // status is actively owned by this tab (queued/running/paused/failed)
    // is never overwritten by a merged job; for anything else the server
    // job's status is authoritative, whatever it says.
    return !['queued', 'running', 'paused', 'failed'].includes(from);
  }
  const actors = FILES_TRANSFER_TRANSITIONS[from]?.[to];
  return Array.isArray(actors) && actors.includes(actor);
}

// Settle the completion promise for a transfer entering `status`:
// completed → resolve(result); failed/cancelled → reject(failure, or an
// Error built from transfer.error). Other statuses settle only when the
// caller hands in the teardown failure — pausing a *running* transfer
// rejects the in-flight attempt (AbortError) while pausing a *queued* one
// leaves the promise pending; Resume mints a fresh promise either way.
function filesTransferFsmSettle(transfer, status, { result, failure } = {}) {
  if (status === 'completed') {
    transfer.resolve?.(result);
  } else if (status === 'failed' || status === 'cancelled') {
    transfer.reject?.(failure !== undefined ? failure : new Error(transfer.error || `transfer ${status}`));
  } else if (failure !== undefined) {
    transfer.reject?.(failure);
  }
}

// Register a newly created entry with the machine: validate its initial
// status for the creating actor, arm the completion promise, and settle it
// immediately when the entry is born terminal (a restored 'completed'
// download, a merged job that already finished) so "terminal ⇒ settled"
// holds from birth. User creations bump queueEpoch — together with the
// Resume/Retry re-arm these are the "queue/resume/retry" bumps the pump
// backstop keys on.
function filesTransferFsmInit(transfer, { actor = 'unknown' } = {}) {
  if (!transfer) return null;
  const status = transfer.status;
  const allowed = FILES_TRANSFER_INITIAL_STATUSES[actor];
  if (!allowed || !allowed.includes(status)) {
    filesTransferFsmReport(
      `illegal initial status '${status}' for actor '${actor}'`,
      transfer,
      { actor, to: status }
    );
  }
  if (actor === 'user') transfer.queueEpoch = (transfer.queueEpoch || 0) + 1;
  filesTransferCompletion(transfer);
  filesTransferFsmSettle(transfer, status, {});
  return transfer;
}

// The single mutation point for transfer.status. Options:
//   actor   — 'user' | 'runner' | 'pump' | 'server'; checked against the
//             table, illegal moves are reported (console.error, counted in
//             window.qa.transfers()) and then applied anyway.
//   error   — string (including '') assigns transfer.error; undefined
//             applies the default (entering queued/running clears it, as
//             every queue/resume/retry/runner-start site does; other
//             targets leave it untouched); null forces leave-untouched
//             even for queued/running (the server merge owns error itself).
//   result  — resolve value for the completion promise when to='completed'.
//   failure — reject error for the completion promise (defaults to an
//             Error from transfer.error for failed/cancelled; also rejects
//             the in-flight attempt on a runner's pause teardown).
//   reason  — free-text context for the illegal-transition report.
//   persist/render — default true; the server-merge loop batches both.
function filesTransferTransition(transfer, to, options = {}) {
  const {
    actor = 'unknown',
    error,
    reason = '',
    result,
    failure,
    persist = true,
    render = true,
  } = options;
  if (!transfer) {
    filesTransferFsmReport(`transition to '${to}' on a missing transfer`, null, { actor, to, reason });
    return null;
  }
  const from = transfer.status;
  if (!FILES_TRANSFER_STATUSES.has(to)) {
    filesTransferFsmReport(`unknown target status '${to}'`, transfer, { actor, from, to, reason });
  } else if (!filesTransferTransitionAllowed(from, to, actor)) {
    filesTransferFsmReport(`illegal transition ${from} → ${to} (actor '${actor}')`, transfer, { actor, from, to, reason });
  }
  transfer.status = to;
  if (to === 'queued') {
    // Re-arm: entering the queue is a fresh attempt. Bump queueEpoch (the
    // pump backstop exempts entries whose epoch moved — re-queues racing a
    // runner teardown are legitimate), clear the teardown flags, and mint a
    // fresh completion promise for the new attempt.
    transfer.queueEpoch = (transfer.queueEpoch || 0) + 1;
    transfer.pauseRequested = false;
    transfer.cancelRequested = false;
    if (error === undefined) transfer.error = '';
    filesTransferCompletion(transfer);
  } else if (to === 'running') {
    transfer.pauseRequested = false;
    transfer.cancelRequested = false;
    if (error === undefined) transfer.error = '';
  }
  if (error !== undefined && error !== null) transfer.error = error;
  if (persist) filesTransferPersistState();
  if (render) renderFilesTransfers();
  filesTransferFsmSettle(transfer, to, { result, failure });
  return transfer;
}

// QA readback (window.qa convention): the transfer table harnesses assert
// on — entry states plus the FSM's illegal-transition counter (0 in a
// healthy session). Probe stays cheap and side-effect-free.
window.qa = Object.assign(window.qa || {}, {
  transfers: () => ({
    entries: filesTransfers.map(transfer => ({
      id: transfer.id,
      kind: transfer.kind,
      status: transfer.status,
      error: transfer.error || '',
      epoch: Number(transfer.queueEpoch || 0),
    })),
    illegalTransitions: filesTransferFsmIllegalCount,
    recentReports: filesTransferFsmRecentReports.slice(),
  }),
});
