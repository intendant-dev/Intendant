#!/bin/bash
# THE canonical WASM artifact builder — the CI drift gate and manual
# rebuilds both come through here, and build.rs mirrors the same flags
# (see the lockstep note at its wasm-pack invocation), so every producer
# emits identical bytes on the canonical host triple (aarch64-darwin).
#
# Why the remap: dependency code compiled into the artifacts embeds
# panic-location paths under the building account's cargo registry
# ($CARGO_HOME/registry/src/...), which made byte equality
# account-dependent — the vm-built artifacts and the CI account's gate
# rebuild differed in exactly those strings (proven 2026-07-11, and
# byte-identity across accounts was re-proven with this remap in
# place). Workspace-relative paths are already relative; the registry
# is the only absolute-path source.
set -euo pipefail
cd "$(dirname "$0")/.."
registry="${CARGO_HOME:-$HOME/.cargo}/registry/src"
export RUSTFLAGS="--remap-path-prefix ${registry}=/cargo/registry/src"
(cd crates/presence-web && wasm-pack build --target web --out-dir ../../static/wasm-web --out-name presence_web)
(cd crates/station-web && wasm-pack build --target web --out-dir ../../static/wasm-station --out-name station_web)
