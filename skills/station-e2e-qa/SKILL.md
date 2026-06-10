---
name: station-e2e-qa
description: Use when validating the Station canvas dashboard end to end — renderer health (WebGPU/fps/frame pacing), agentic canvas interaction via station.debug_json()/station.activate(), and the perf regression gate — with scripts/validate-dashboard.cjs on a headless CI-style box or a headed GPU host. Covers environment setup, the dual-GPU Chromium recipe, headless gate vs headed acceptance invocations, the --station-perf-eval before/after workflow, and portal-grant caveats for display streams.
---

# Station E2E QA

`scripts/validate-dashboard.cjs` is the zero-dependency Node+CDP harness for Station QA.
It launches Chromium, opens the dashboard's Station tab, runs named probes, and prints one
PASS/FAIL line (or one JSON object with `--json`). Run `--self-test` first on a new box; it
needs no browser and validates the harness itself.

All commands below run from your own worktree (never the main worktree), against a
throwaway port — the helper refuses the protected port 8765.

## Environment On The GPU Host

Non-login SSH shells on the GPU host have neither the graphical session nor the Rustup
toolchain. For `--headed` runs on Linux the validator auto-imports `DISPLAY`,
`WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, `XAUTHORITY`, and friends
from `systemctl --user show-environment` when `DISPLAY`/`WAYLAND_DISPLAY` are absent — but
the user session must actually be live (GNOME/RDP), and `XAUTHORITY` matters: Chromium
under ozone-x11 exits with "Authorization required" without it. Keep the PATH fix explicit:

```sh
ssh user@<gpu-host> '
  set -euo pipefail
  cd <worktree>
  export PATH="$HOME/.cargo/bin:$PATH"
  [ -x target/release/intendant ] || cargo build --release -p intendant
  ...
'
```

If Chromium dies before CDP with `Missing X server or $DISPLAY`, the graphical session
env is still missing; the validator's failure output points back to
`systemctl --user show-environment`.

## Dual-GPU Chromium Recipe (Intel + NVIDIA)

Documented in detail in `skills/wayland-portal-e2e/SKILL.md` ("GPU Selection On Dual-GPU
Hosts"); summary: Wayland-ozone is incompatible with Chromium's Vulkan path, so force X11
ozone plus Vulkan and pin the NVIDIA ICD. With the validator, the env vars go in the
environment and the Chromium flags via `--browser-arg` (`--station-probe webgpu` already
implies `--enable-gpu` and adds `--enable-unsafe-webgpu`):

```sh
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/nvidia_icd.json
node scripts/validate-dashboard.cjs ... \
  --headed \
  --browser-arg=--ozone-platform=x11 \
  --browser-arg=--enable-features=Vulkan
```

Hardware WebGPU is confirmed when the `webgpu` probe passes; if in doubt, check that
`navigator.gpu.requestAdapter({powerPreference:'high-performance'})` reports an `nvidia`
adapter, not `intel` or `swiftshader`.

## Headless Gate (any box, no GPU)

Fast pre-merge gate; runs fine over plain SSH:

```sh
node scripts/validate-dashboard.cjs \
  --launch-dashboard --port 8898 \
  --dashboard-arg --no-presence \
  --check-static-scripts \
  --station-probe status,rendered,fps,debug-json \
  --json
```

`fps` reads the Station-presented `fps=NN` eval from `station.debug_state()`
(`--station-min-fps`, default 24). `debug-json` soft-passes with
`supported=false` on builds that predate `station.debug_json()`; add
`--require-debug-json` once the export is expected. The `smooth` probe also works
headless, but headless rAF pacing throttles under host load — treat headless `smooth`
failures as advisory and gate frame pacing on the headed run instead.

## Headed GPU Acceptance (the one command)

Full renderer + interaction + perf acceptance on the GPU host:

```sh
node scripts/validate-dashboard.cjs \
  --launch-dashboard --port 8897 \
  --headed \
  --browser-arg=--ozone-platform=x11 \
  --browser-arg=--enable-features=Vulkan \
  --station-probe rendered,webgpu,fps,smooth,debug-json \
  --station-interaction-probe \
  --station-perf-eval \
  --screenshot /tmp/station-acceptance.png \
  --timeout 30000 --dashboard-timeout 30000 \
  --json
```

- `smooth` samples ~2s of `requestAnimationFrame` deltas in-page and fails when the p95
  frame gap exceeds `--station-max-frame-gap` (default 40ms) or any gap exceeds 250ms;
  it reports `{fps, p50, p95, worst}` and catches main-thread stalls the fps figure hides.
- `--station-interaction-probe` keeps end-to-end pointer-path coverage: it clicks the
  rendered hotspot buttons and regex-verifies `Opening <kind>` in `#station-status`,
  reporting warm-up vs subsequent latency.
- On builds exporting `station.activate`, prefer `--station-activate NAME` (repeatable)
  for target-level checks: it activates programmatically and verifies via
  `debug_json` `selectedId` or the status text — faster and immune to hotspot geometry.
- Probe reports, interaction/activation latencies, and the perf report all land in the
  single JSON result (`stationProbeReports`, `stationInteraction`, `stationActivation`,
  `stationPerfEval`).

## Perf Regression Gate (`--station-perf-eval`)

Run before and after any renderer/display change and diff the two reports. The scripted
sequence: open Station, settle 1.5s, idle `smooth` sample, activate three targets
(default `system:activity`, `system:controls`, `system:view`, or each `--station-activate`
NAME) timing each, then a second `smooth` sample — with display streams playing if any are
granted. It emits one JSON report:

```json
{"fpsIdle":60,"fpsAfterInteraction":58,"p95Idle":17,"p95Active":21,
 "interactionLatencies":[38,12,11],"interactionInput":"wasm-activate",
 "displays":1,"failures":[],"verdict":"pass", ...}
```

Verdict fails on: fps below `--station-min-fps` in either sample, p95 above
`--station-max-frame-gap`, any gap above 250ms, a stalled sample, or a failed activation
(`failures` lists every violation; the run exits non-zero with kind `perf-eval`). On
builds without `station.activate` the eval falls back to click activation automatically.

```sh
node scripts/validate-dashboard.cjs --launch-dashboard --port 8897 --headed \
  --browser-arg=--ozone-platform=x11 --browser-arg=--enable-features=Vulkan \
  --enable-gpu --station-perf-eval --json > /tmp/perf-after.json
diff <(jq .stationPerfEval /tmp/perf-before.json) <(jq .stationPerfEval /tmp/perf-after.json)
```

Compare absolute numbers, not just the verdict — a 60→42fps drop still "passes" defaults.

## Driving The Canvas UI From debug_json

The Station is a WASM canvas — there is no DOM to query inside it. New builds export a
structured probe API on the page-global `station` handle (feature-detect each; absent on
older builds):

- `station.debug_json()` → object (or JSON string) with `fps`, `renderer`, `gpu`,
  `hosts`, `agents`, `events`, `displays`, `selectedId`, `layout`, `mood`, `motion`,
  `hitZones: [{name,x,y,w,h}]`, `systemTargets: [...]`.
- `station.activate(name)` → bool; selects/opens a target programmatically.
- `station.hotspot_rects()` → pixel-space rects when pointer-path clicks are needed.

The agentic loop: read `debug_json()` → pick a `hitZones`/`systemTargets` name → call
`station.activate(name)` → confirm `debug_json().selectedId` (or the status text) reflects
it. Through the validator that is exactly `--station-probe debug-json` (read) plus
`--station-activate NAME` (act + verify):

```sh
node scripts/validate-dashboard.cjs --port 8897 \
  --station-probe debug-json --require-debug-json \
  --station-activate system:controls --station-activate system:view \
  --json | jq '.stationProbeReports["debug-json"].data, .stationActivation'
```

For ad-hoc reads in an existing run, `--wait-for-function` can evaluate any expression on
the page, e.g. `--wait-for-function "() => station && station.debug_json && station.debug_json().fps >= 24"`.

## Portal-Grant Caveats (display streams)

`displays` in `debug_json` and the "with displays" leg of the perf eval only exercise real
WebRTC streams when a user display has been granted. On Wayland hosts the grant goes
through the XDG Desktop Portal dialog, which can re-prompt for a fresh Intendant instance
and cannot be approved from a bare SSH shell — follow `skills/wayland-portal-e2e/SKILL.md`
for the GNOME Remote Desktop approval flow and its safety boundary. Without a grant the
QA run is still valid; it simply measures the no-stream Station, so record whether
displays were present (the perf report includes `displays` when `debug_json` is
available) before comparing runs.

## Cleanup And Etiquette

`--launch-dashboard` owns its temporary dashboard and Chromium profile and tears both
down (add `--keep-browser`/`--keep-artifacts` to debug). Check ports before launching;
never target 8765 and never kill Intendant instances you did not spawn:

```sh
ss -ltnp | grep -E ':(8897|8898)\b' || true
pgrep -af intendant || true
```
