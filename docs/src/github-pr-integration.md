# GitHub PR Integration

Intendant watches your repositories' pull requests through a dedicated
**GitHub App** — a real App identity with fine-grained, **read-only**
permissions and short-lived installation tokens, never a personal
access token and never a `gh` CLI wrapper. This chapter covers the
integration's trust shape and the five-minute setup ceremony. (The
coordination radar's cheap `gh pr list` file-set read is a separate,
unrelated lane and keeps working with or without this integration.)

Configured, the daemon's **PR scanner** mirrors every watched pull
request onto the agenda as a **thin intent anchor** under a "PRs" hub,
and the dashboard joins live PR state onto those anchors **at render
time** — GitHub stays the sole store of PR state, and no state change
is ever an agenda op.

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

## The render-time join

Live PR state is served **beside** the agenda, never on it, in two
tiers:

- **Tier 1** — open state, draft flag, live title, branches, author —
  is what the scanner's list poll already returned. It rides the agenda
  snapshot response as a `pull_requests` sibling map keyed by the
  anchors' url-ref locators (the sessions-join shape: the item DTO
  stays the pure fold product; a locator with no entry claims nothing).
  Cards chip from it — a `draft` badge, a `renamed` badge when the PR's
  live title diverged from the parked one (the anchor is never
  patched) — at zero per-render cost: the poll fetched it, not the
  render.
- **Tier 2** — checks rollup, review rollup, mergeability — is fetched
  through a daemon-side cache when a card's inspector opens
  (`GET /api/agenda/items/{item_id}/pr-state`, tunnel twin
  `api_agenda_pr_state`): single-flight per PR so ten open dashboards
  cost one GitHub exchange, a 60-second freshness floor so re-expands
  are free, bounded retention. Every rendered state carries its age
  ("as of 40s ago").

Degrade is honest and boring: integration unconfigured or paused, key
denied, GitHub unreachable, PR vanished — the card shows the anchor
plus "state unavailable", never an error. The daemon holds the one
GitHub client and the one rate budget; the browser never sees a GitHub
credential and no CORS surface exists.

Operator acceptance probe (headless, against a configured daemon or a
fixture rig):

```bash
node scripts/validate-dashboard.cjs --port <port> \
  --wait-for-selector '#tab-agenda' \
  --probe-json "prjoin=window.qa.agendaPrJoin()"
```

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
- **A rebuilt binary re-asks the macOS Keychain — once.** The custody
  entry's ACL names the binary that created it; after any rebuild the
  first custody touch makes macOS raise a SecurityAgent authorization
  ("intendant-bin wants to use your confidential information…"), which
  can hide behind windows or on another desktop. Click **Always Allow**
  for the new binary. While the dialog waits, the parked custody call
  runs on a surrendered runtime slot (`custody_blocking`,
  `block_in_place`) so the daemon keeps serving, and the dashboard's
  GitHub section names the situation after 20 s instead of showing an
  eternal progress line. Development builds re-prompt on every rebuild;
  signed releases keep one stable identity (see the TCC/signing notes
  in the release docs).
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
- **The one-click ceremony's guard is a daemon-minted single-use
  state.** GitHub's redirect lands on an authority-free callback route
  (a browser navigation carries no daemon credential); without the
  matching unexpired state — 32 random bytes, stored hashed, burned
  atomically before any GitHub call — the callback is a uniform refusal
  and the code is **never exchanged**, so a foreign code can never
  substitute attacker-App credentials. The state travels only inside
  the GitHub form action and its redirect; the per-boot dashboard
  admission token never rides the redirect URL.
- **Intendant stores only what the daemon uses.** The conversion
  response's `client_secret` and `webhook_secret` are discarded at
  parse (the daemon retains the App id, slug, and private key — nothing
  else); the PEM travels GitHub → daemon → custody without ever
  touching a file, a log, or the browser.

## Setup: one click

You (the owner) do this once; agents cannot and must not. Dashboard →
**Vault** → *GitHub App integration*:

1. **Connect GitHub** — optionally type your org handle (blank = your
   personal account) and press **Connect GitHub**. Your browser goes to
   github.com with a pre-filled manifest: a **private** App named
   `Intendant (<host>)` with exactly the three read-only permissions
   and **no webhook**. One green button — *Create GitHub App* — and
   GitHub sends the browser back to the daemon, which exchanges the
   single-use code server-side and **seals the App's key straight into
   custody**. You never see, download, or paste a PEM. (If the name is
   taken, GitHub's own page lets you edit it before creating.)
2. **Install on GitHub** — the section now shows the sealed App with an
   *Install on GitHub* link. Pick the account and the repositories the
   App may see. Back in the dashboard, the installation is discovered
   automatically within a few seconds (several installations render a
   picker; none after two minutes leaves the link plus the manual
   fallback).
3. **Pick repositories** — tick the repos to watch and *Save selection
   & verify*. The daemon performs one real exchange (token mint plus a
   pull list) so the status chip says `valid` — or tells you exactly
   why not. Poll cadence lives under *Advanced* (default 5 minutes,
   floor 1).

Status states are honest and few: `unconfigured` (nothing runs),
`configured` (sealed, no exchange yet), `valid`, `unreachable`
(network/rate trouble, self-healing), `denied` (bad or revoked
credentials — fix the configuration). A `pending install` chip rides
beside them between Create and Install. One transient to know: after a
daemon restart mid-pending, the chip reads plain `configured` until the
section's first discovery call (or a scanner pass) re-establishes the
pending phase — status polls never unseal the document, by design.
Remove deletes the sealed entry (idempotent, audited as
`key_custody_removed`) and returns the integration to `unconfigured`.

The effective watch set is the intersection of your configured list and
the installation's repositories — a repo listed but not installed (or
vice versa) simply isn't watched, and the status surface shows the
configured list so nothing skips silently.

The ceremony needs the browser to reach the daemon's own origin
(loopback or a directly-reached TLS dashboard) and a custody backend on
the daemon (macOS today — the Connect button refuses by name
otherwise). Where either is missing, the manual fallback below is the
path.

## Fallback: the manual five-field ceremony

The form under *Enter credentials manually* accepts everything the
one-click path automates — it remains fully supported:

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
4. **Enter it in the Vault tab** — paste the App ID, installation id,
   and the `.pem` contents; list the repos to watch (`owner/repo`, one
   per line); *Save & verify*. Delete the downloaded `.pem` afterwards;
   the sealed copy is now the working one.

## Endpoints

`POST /api/integrations/github` (configure; `credentials.manage`),
`GET /api/integrations/github/status` (`settings`),
`DELETE /api/integrations/github` (`credentials.manage`),
`GET /api/integrations/github/installations` and
`GET /api/integrations/github/repositories` (both `credentials.manage`
— they unseal the key to mint tokens) — declared on the gateway route
table with dashboard-tunnel twins (`api_github_integration_save` /
`_status` / `_remove`, `api_github_installations` /
`api_github_repositories`). The ceremony's two HTTP-only routes carry
no tunnel twin by design: `POST
/api/integrations/github/manifest-start` (`credentials.manage`; its
payload is only meaningful to a browser on the daemon's own origin) and
`GET /api/integrations/github/callback` (the authority-free redirect
landing — the single-use state is the authorization).
