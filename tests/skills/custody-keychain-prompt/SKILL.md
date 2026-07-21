---
name: custody-keychain-prompt
description: Operator-hardware prompt lane for Track K custody — verifies the interactive macOS keychain arc (Always Allow on the wrapping key, silent reads after, deny on Deny) that the hermetic acceptance rig deliberately excludes
---

# Custody keychain prompt lane (macOS, operator hardware only)

The hermetic acceptance rig (`crates/intendant-custody/tests/mac_acceptance.rs`)
proves caller discrimination with keychain UI **disabled**: the creating binary
reads silently, an unregistered binary lands on `DeniedNonInteractive`. What it
deliberately cannot exercise is the interactive third path — SecurityAgent
showing the wrapping-key prompt and a human ruling on it. That lane needs a GUI
session and a person, so it lives here (Track K ruling Q7: prompt lane =
operator hardware, never CI).

**Requires:** a macOS GUI session (SecurityAgent prompts are invisible over
SSH — they appear on the console), a build of this worktree, and a daemon whose
access certs exist (`intendant access setup` has been run). Use a scratch
`$HOME`-adjacent daemon if you don't want to touch your live estate; the flow
below relocates real keys and restores them at the end.

## Arc

1. **Migrate (silent — creating caller):**
   `target/release/intendant custody migrate`
   Expect per-file `done` lines, the *relocation-not-rotation* label, and **no
   prompt** — the migrating binary mints the wrapping-key item, so the item ACL
   trusts it from birth. `intendant custody status` shows every artifact as
   `custody (sealed blob present)`.
2. **Silent reads (same binary):** restart the daemon from the same build;
   HTTPS bind (server.key) and any peer dial (client.key) must proceed with no
   prompt and no new `key_custody_denied` events in the custody trail.
3. **The prompt (different binary identity):** rebuild the binary (any code
   change re-signs the ad-hoc identity), run
   `target/release/intendant custody status`, then trigger a read (restart the
   daemon or `intendant access serve-certs`). SecurityAgent shows the
   wrapping-key prompt for `dev.intendant.custody`:
   - **Always Allow** → this identity joins the item ACL; subsequent reads are
     silent. (On signed installs the stable "Intendant Dev" identity makes this
     a once-ever event; ad-hoc dev builds re-prompt per rebuild — the ruled Q2
     reason custody is recommended-by-default on signed installs only.)
   - **Deny** → the read fails with the named custody error, and
     `key_custody_denied` lands in the trail. The daemon must fail closed with
     the `intendant custody restore` guidance, not hang and not fall back to
     any file.
4. **Headless cross-check:** from an SSH session (no GUI), a read by a
   not-yet-allowed binary must return the `DeniedNonInteractive` class
   promptly — never park waiting on a prompt nobody can see.
5. **Restore:** `intendant custody restore` — files return, blobs deleted,
   `key_custody_restored` events in the trail, daemon boots clean from files.

Record outcomes (prompt shown? silence after Always Allow? deny named?) in the
session notes; this lane is the human proof behind the docs' custody claims.
