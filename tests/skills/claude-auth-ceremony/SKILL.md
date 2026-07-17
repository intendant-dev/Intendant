---
name: claude-auth-ceremony
description: >
  Operator-only manual scenario for the dashboard-guided Claude sign-in
  ceremony (/api/claude-auth/*): start it against the real `claude` CLI,
  verify the sign-in URL capture (PATH shim primary, PTY parse fallback),
  the paste-prompt state, cancel-path non-destructiveness, the 5-minute
  timeout reap, and the custody/tier refusals. NEVER completes a real
  login — the cancel path is the exercised path.
compatibility: OPERATOR-ONLY, never CI. Requires the `claude` CLI (≥ 2.1.212)
  on PATH, a running daemon built from this tree, and a trusted local (or
  direct-mTLS) dashboard session. Interactive; touches the machine's real
  Claude credential store only if you disobey the "never complete the login"
  rule.
allowed-tools: Bash Read
disable-model-invocation: true
---

# Claude Sign-In Ceremony — Manual Scenario (operator-only)

## Ground rules

- **Never complete a real login during this scenario.** Cancel is the
  verified-safe exit (credential store untouched; a prior login keeps
  working). Completing a login changes the Claude account for every
  Claude Code session on the machine.
- Run against a scratch daemon from your own worktree, never the repo
  root.

## Steps

1. **Baseline**: `claude auth status` — note `loggedIn` / `email`. This
   must be unchanged at the end.
2. **Start**: on the dashboard, Vault tab → "Claude account — this
   daemon" → Start Claude sign-in (or
   `curl -s -X POST localhost:<port>/api/claude-auth/start -d '{}'`
   from the daemon box). Expect phase `starting` →
   `awaiting_browser`/`awaiting_code` within ~2s.
3. **URL capture**: status must carry a `url` on claude.com with
   `code=true…code_challenge…state` intact. Confirm **no browser opened
   on the daemon box** (the PATH shim swallowed `open`/`xdg-open`) and
   that `~/.intendant/claude-auth/` holds one `shim-*` dir while active.
4. **Busy refusal**: a second start must 409.
5. **Code validation**: submit `two tokens` → 400; submit a plausible
   single token → phase `verifying` (the CLI will reject it — expect a
   terminal `failed` with honest copy, or cancel first).
6. **Cancel**: start again, then Cancel. Expect phase `cancelled`, the
   `claude` process gone (`pgrep -f "auth login"`), the shim dir
   deleted, and `claude auth status` identical to the baseline.
7. **Timeout** (optional, 5 min): start and walk away; expect
   `timed_out`, process reaped, shim dir deleted.
8. **Custody gate**: with an active `oauth:claude-code` lease (fuel from
   the vault) or a registered anthropic client-egress relay, start must
   403 with the custody copy. Drop the lease/relay; start works again.
9. **Log hygiene**: grep the daemon log and the session logs for the
   ceremony window — the sign-in URL's `state`/`code_challenge` values
   and any pasted code must appear NOWHERE.
10. **Reload chips** (needs a running supervised Claude Code session):
    after any terminal state, the success panel is not shown — to see
    the chips without a real login, temporarily inspect
    `/api/claude-auth/status` after a real (deliberate, operator-owned)
    login on a scratch account, or verify the chip path directly:
    `intendant ctl` → send `{"action":"reload_credentials","session_id":"<id>"}`
    and confirm the session's backend restarts resume-attached (same
    conversation), a parked rate-limit timer cancels, and queued
    messages re-deliver after the respawn.
