// ── ui-v2 Activity slice 1: approval-card semantics (user-approved) ────
// Under the flag, the approval card's bulk action becomes CATEGORY-
// SCOPED: "Approve all <category>" sets that category's rule to auto via
// the shipped set_approval_rule machinery, then approves the pending
// command. The old approve_all (which flips autonomy to Full) stays
// available but relabeled to say what it does. The `a` hotkey follows
// the new semantics under the flag (capture phase; v1 handler untouched
// and still the default without the flag).

function ui2ApprovalCategory() {
  // stationCurrentApproval is the cross-module source of truth for the
  // pending approval; the panel's category line is the fallback.
  const fromGlobal = (typeof stationCurrentApproval !== 'undefined' && stationCurrentApproval &&
    (stationCurrentApproval.category || stationCurrentApproval.action_category)) || '';
  if (fromGlobal) return String(fromGlobal);
  const el = document.getElementById('approval-category');
  const m = /([a-z_]+)\s*$/i.exec((el && el.textContent) || '');
  return m ? m[1] : '';
}

function ui2ApproveCategoryRule() {
  const category = ui2ApprovalCategory();
  if (category && typeof dispatchControlMsg === 'function') {
    dispatchControlMsg({ action: 'set_approval_rule', category, rule: 'auto' });
  }
  sendApproval('approve');
}

function ui2AugmentApprovalPanel() {
  const actions = document.querySelector('#approval-panel .approval-actions');
  if (!actions || document.querySelector('.ui2-approve-category')) return;
  const buttons = [...actions.querySelectorAll('button')];
  const allBtn = buttons.find((b) => /approve all/i.test(b.textContent));
  const approveBtn = buttons.find((b) => b.classList.contains('approve'));

  const catBtn = document.createElement('button');
  catBtn.type = 'button';
  catBtn.className = 'ui2-approve-category';
  catBtn.innerHTML = 'Approve all like this <kbd>a</kbd>';
  catBtn.title = 'Set this approval category to auto-approve (a shipped per-category rule), then approve this command. Narrower than switching autonomy.';
  catBtn.addEventListener('click', ui2ApproveCategoryRule);
  if (approveBtn && approveBtn.nextSibling) actions.insertBefore(catBtn, approveBtn.nextSibling);
  else actions.appendChild(catBtn);

  if (allBtn) {
    allBtn.classList.add('ui2-full-escape');
    allBtn.innerHTML = 'Switch to Full autonomy';
    allBtn.title = 'The previous "Approve all": flips autonomy to Full — everything runs unattended from here.';
  }

  // `a` follows the category semantics under the flag. Capture phase so
  // the v1 shortcut handler (which would call approve_all → Full) never
  // sees the key while an approval is pending.
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'a' || e.metaKey || e.ctrlKey || e.altKey) return;
    const tag = (e.target && e.target.tagName) || '';
    if (/INPUT|TEXTAREA|SELECT/.test(tag) || (e.target && e.target.isContentEditable)) return;
    const panel = document.getElementById('approval-panel');
    if (!panel || getComputedStyle(panel).display === 'none') return;
    e.preventDefault();
    e.stopPropagation();
    ui2ApproveCategoryRule();
  }, true);
}

if (ui2Enabled()) {
  const wire = () => ui2AugmentApprovalPanel();
  document.addEventListener('DOMContentLoaded', wire, { once: true });
  if (document.readyState !== 'loading') wire();
}
