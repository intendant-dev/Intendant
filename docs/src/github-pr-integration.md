# GitHub PR Integration

Intendant watches your repositories' pull requests through a dedicated
**GitHub App** — a real App identity with fine-grained, **read-only**
permissions and short-lived installation tokens, never a personal
access token and never a `gh` CLI wrapper. This chapter covers the
integration's trust shape and the five-minute setup ceremony. (The
coordination radar's cheap `gh pr list` file-set read is a separate,
unrelated lane and keeps working with or without this integration.)

Configured, the daemon's **PR scanner** mirrors every watched pull
request onto the agenda as a **thin intent anchor** under a "PRs" hub;
the render-time state join (checks/review/mergeable on card expand)
lands in the next slice.

## The scanner and its anchors

The scanner is a deterministic, zero-LLM daemon task (the coordination
radar's sibling — that radar's cheap `gh` lane is separate and
unchanged). Every poll (default 5 minutes, ETag-conditional,
`[integrations.github] poll_minutes` overrides with floor 1) it diffs
the watched repos' open-PR sets against its own anchors and writes
**intent lifecycle ops only**:

- **A new PR** parks one `task` anchor — title `Repo#N: <PR title>`,
  a one-line body of immutable facts (author, `base ← head`), the tag
  `pr`, and a single `url` ref to the PR (the join key) — and files it
  under the **PRs hub** (an ordinary note titled "PRs", keyed by the
  reserved `prs-hub` tag, created on the first scan that needs it).
  On first enable this backfills every currently-open PR (drafts
  included — draft-ness is mutable state and lives in the render join,
  never in tags or titles); historical closed PRs are never parked.
- **A merged or closed PR** gets a terminal annotation ("PR merged
  (abc123def) at …" / "PR closed without merge at …"), is unfiled from
  the hub, and completed. The hub is therefore the **open-PRs shelf** —
  live children stay at dozens, and the log keeps the placement
  history.
- **A reopened PR** resurrects its own anchor (reopen + re-file) — no
  duplicate is ever parked while an anchor of any status exists for
  `repo#number`. **A retired anchor stays retired: an owner act
  outranks the mirror; the scanner annotates instead** (once).

PR *state* — checks, reviews, draft flips, title edits, mergeability —
is **never an agenda op**: the diary stays byte-quiet while GitHub
churns (pinned by test). Anchors point, never store: a `url` ref and a
title suffice; GitHub remains the sole store of PR state.

Every scanner write is attributed to the **`daemon` actor kind** (the
daemon itself, on schedule — unmintable by any external surface) with
the self-described source label `github-pr-scanner` beside it, and
scanner anchors that are currently filed are **exempt from the triage
frontier** by definition — they arrive placed and described; unfiling
one re-admits it.

The scanner keeps no durable state: the agenda is the durable truth,
GitHub is the live truth, scanner state is a pure function of both — a
daemon restart re-derives everything and converges without duplicates.

**This does not replace the closed-loop landing watchers.** A session
actively landing a PR watches the merge queue at seconds freshness
with its own watcher; the scanner is minutes-fresh *ambient* awareness
for every PR on the fleet, watched or not. Both exist on purpose.

## Trust shape

- **Credentials live in daemon custody, not files.** The App's private
  key, App ID, and installation id seal together into one OS-keystore
  custody entry (`github-app/credentials`) — *born in custody*: the key
  never exists as a plaintext file, and there is deliberately no
  env-var or file fallback lane for this class. A custody denial means
  the integration is off (named, audited), never served stale. The
  Track K label applies here as everywhere: custody is bar-raising, not
  lane-sealing, until signed installs land. On platforms without a
  custody backend yet (Windows/Linux until their Track K slices), the
  configure gesture fails with a named error rather than degrading to a
  file.
- **Read-only by construction and by permission set.** The App needs
  exactly: `Metadata: read` (baseline), `Pull requests: read`,
  `Checks: read`. Nothing else — no Contents, no Issues, no write
  permission of any kind. The integration never writes to GitHub.
- **Status never unseals.** The status surface answers from blob
  existence plus the cached outcome of the last real exchange; the key
  is unsealed only to mint the App JWT (roughly once an hour under the
  installation token's lifetime).
- **Polling is conditional and rate-honest.** List reads carry ETags
  (a 304 costs nothing), failures back off, and rate-limit headers are
  honored. Configuration state (which repos to watch, the poll cadence)
  is non-secret and lives in `[integrations.github]` in
  `intendant.toml`.

## Setup: the five-minute ceremony

You (the owner) do this once; agents cannot and must not.

1. **Register the App** — GitHub → your org (or user) → Settings →
   Developer settings → GitHub Apps → *New GitHub App*. Name it
   something like `intendant-<yourorg>`; Homepage URL can be the repo.
   **Uncheck Webhook → Active** (v1 polls; the daemon has no public
   endpoint). Under *Permissions → Repository permissions* grant
   exactly: **Metadata: Read-only**, **Pull requests: Read-only**,
   **Checks: Read-only**. "Where can this App be installed?" — *Only on
   this account*.
2. **Generate the private key** — on the new App's page, *Generate a
   private key*. GitHub downloads a `.pem` file; note the **App ID**
   shown at the top of the same page.
3. **Install the App** — *Install App* in the App's sidebar → choose
   the account → *Only select repositories* → pick the repos to watch.
   After installing, the browser URL ends in the **installation id**
   (`…/settings/installations/<number>`).
4. **Enter it in the Vault tab** — dashboard → **Vault** → *GitHub App
   integration*: paste the App ID, installation id, and the `.pem`
   contents; list the repos to watch (`owner/repo`, one per line);
   *Save & verify*. The key seals into custody and the daemon performs
   one real exchange (token mint plus, when repos are listed, a pull
   list) so the status chip says `valid` — or tells you exactly why
   not. Delete the downloaded `.pem` afterwards; the sealed copy is now
   the working one.
5. **Done.** Status states are honest and few: `unconfigured` (nothing
   runs), `configured` (sealed, no exchange yet), `valid`,
   `unreachable` (network/rate trouble, self-healing), `denied` (bad or
   revoked credentials — fix the configuration). Remove deletes the
   sealed entry (idempotent, audited as `key_custody_removed`) and
   returns the integration to `unconfigured`.

The effective watch set is the intersection of your configured list and
the installation's repositories — a repo listed but not installed (or
vice versa) simply isn't watched, and the status surface shows the
configured list so nothing skips silently.

## Endpoints

`POST /api/integrations/github` (configure; `credentials.manage`),
`GET /api/integrations/github/status` (`settings`),
`DELETE /api/integrations/github` (`credentials.manage`) — declared on
the gateway route table with dashboard-tunnel twins
(`api_github_integration_save` / `_status` / `_remove`).
