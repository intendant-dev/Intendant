#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
DIR=$(mktemp -d "${TMPDIR:-/tmp}/agent-view-tests.XXXXXX")
trap 'rm -rf "$DIR"' EXIT
mkdir -p target/agent-view-proof
xcrun swiftc -swift-version 5 -warnings-as-errors macos-app/tests/agent-view/main.swift macos-app/AgentViewModel.swift macos-app/AgentViewTransport.swift macos-app/AgentView.swift -framework Cocoa -o "$DIR/tests"
"$DIR/tests" "$@"
