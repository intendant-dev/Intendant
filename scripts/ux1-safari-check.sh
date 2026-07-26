#!/usr/bin/env bash
# UX1 WebKit acceptance — scripted Safari over the HTTP-fallback lane
# (the exact lane the owner's finding 4 lived in: WebKit cannot
# client-cert WS, so Safari renders the dashboard over plain HTTP with
# the polling event lane).
#
# Drives the REAL Safari at the given dashboard URL (normally the rig's
# `http://127.0.0.1:<port>/?token=<tok>&ceremony=github#vault`) and
# verifies, in the live DOM:
#   1. the ceremony-return marker was consumed (location.search clean);
#   2. the GitHub section mounted and sits in the viewport
#      (return-to-context — no root-drop);
#   3. the served state renders through the pinned feedback grammar
#      (is-progress / is-attention / is-success / is-refusal), and the
#      warning class never co-occurs with is-progress;
#   4. the three non-live states (pending-install, valid, denied)
#      render the grammar correctly when injected — render-truth for
#      states a keyless rig cannot reach (the LIVE ceremony replay is
#      the owner's named acceptance, not this script's).
#
# Requires Safari's "Allow JavaScript from Apple Events" (Develop
# menu). When unavailable this prints the named manual check and exits
# 2 — never a silent pass (UX0 ruling: no silent Chromium-only
# acceptance).
set -euo pipefail

URL="${1:?usage: ux1-safari-check.sh <dashboard-url> (rig URL with ?token=…&ceremony=github#vault)}"

PROBE_JS='(() => {
  const out = { search: location.search, hash: location.hash };
  const mount = document.getElementById("access-github-integration-section");
  if (!mount) return JSON.stringify({ ...out, section: "absent" });
  const rect = mount.getBoundingClientRect();
  out.section = "mounted";
  out.inViewport = rect.top >= -40 && rect.top < window.innerHeight;
  out.liveChips = [...mount.querySelectorAll(".vault-chip")].map((c) => c.className.replace("vault-chip", "").trim() + "|" + c.textContent);
  const states = {
    pending: { configured: false, pending_install: true, app_slug: "intendant-qa", status: "unconfigured", repos: [] },
    valid: { configured: true, status: "valid", repos: ["o/r"], poll_minutes: 5 },
    denied: { configured: true, status: "denied", detail: "installation suspended", repos: [] },
  };
  out.injected = {};
  const saved = githubIntegrationState.data;
  const savedFetched = githubIntegrationState.fetchedAt;
  for (const [name, data] of Object.entries(states)) {
    githubIntegrationState.data = data;
    githubIntegrationState.fetchedAt = Date.now() + 60000;
    renderGithubIntegrationSection();
    out.injected[name] = {
      chips: [...mount.querySelectorAll(".vault-chip")].map((c) => c.className.replace("vault-chip", "").trim() + "|" + c.textContent),
      primaryInstall: !!mount.querySelector(".vault-install-link.primary"),
    };
  }
  githubIntegrationState.data = saved;
  githubIntegrationState.fetchedAt = savedFetched;
  renderGithubIntegrationSection();
  out.warnProgressViolation = !!document.querySelector(".vault-chip.warn.is-progress, .is-progress.warn");
  return JSON.stringify(out);
})()'

if ! RESULT=$(osascript - "$URL" "$PROBE_JS" << 'OSA' 2>&1
on run argv
  set theUrl to item 1 of argv
  set theJs to item 2 of argv
  tell application "Safari"
    activate
    make new document with properties {URL:theUrl}
    delay 8
    set jsResult to do JavaScript theJs in front document
    return jsResult
  end tell
end run
OSA
); then
  echo "SAFARI-UNAVAILABLE: $RESULT"
  cat <<'MANUAL'
NAMED MANUAL CHECK (do this in Safari by hand — the honest fallback):
  1. Enable Develop → Allow JavaScript from Apple Events, then rerun; OR
  2. Open the URL in Safari yourself and verify: the page lands on the
     Vault tab with the GitHub section in view; the address bar shows no
     ?ceremony= leftover; the section's chips read sanely (no amber
     "pending install" once connected; refusals rose and named); the
     "Install on GitHub" action is the big primary button.
MANUAL
  exit 2
fi

echo "SAFARI-PROBE: $RESULT"
python3 - "$RESULT" <<'PY'
import json, sys
r = json.loads(sys.argv[1])
fails = []
if r.get("section") != "mounted": fails.append("github section did not mount")
if "ceremony=" in r.get("search", ""): fails.append("ceremony marker not stripped: " + r["search"])
if not r.get("inViewport"): fails.append("section not scrolled into viewport")
inj = r.get("injected", {})
pend = " ".join(c for c, _t in (x.split("|", 1) for x in inj.get("pending", {}).get("chips", [])))
if "is-attention" not in pend: fails.append("pending-install lost is-attention")
if not inj.get("pending", {}).get("primaryInstall"): fails.append("install action not primary")
val = " ".join(c for c, _t in (x.split("|", 1) for x in inj.get("valid", {}).get("chips", [])))
if "is-success" not in val: fails.append("valid state lost is-success")
den = " ".join(c for c, _t in (x.split("|", 1) for x in inj.get("denied", {}).get("chips", [])))
if "is-refusal" not in den: fails.append("denied state lost is-refusal")
if r.get("warnProgressViolation"): fails.append("warn co-occurs with is-progress in the DOM")
if fails:
    print("UX1-SAFARI FAIL:", "; ".join(fails)); sys.exit(1)
print("UX1-SAFARI PASS: return-to-context + grammar verified on the WebKit lane")
PY
