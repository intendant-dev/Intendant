#!/usr/bin/env python3
"""Compile native background-CU modules without building the whole daemon.

This is a COMPONENT check, not an end-to-end desktop acceptance test. It includes
production AX, native dispatch, target validation and process identity sources.
Unchanged CU data types are extracted verbatim. Screenshot transport and text
formatting are stubbed; no GUI API is invoked by the selected tests. It inherits
all Cargo/compiler-governor environment settings. Full daemon checks still run
on the remote/CI lane.
"""
from __future__ import annotations
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def item(source: str, name: str) -> str:
    match = re.search(r"^pub (?:enum|struct) " + re.escape(name) + r" \{.*?^\}", source, re.M | re.S)
    if not match:
        raise ValueError(f"could not find production type {name}")
    return match.group(0)


def main() -> int:
    if platform.system() != "Darwin":
        print("This native component check requires macOS.", file=sys.stderr)
        return 2
    if not (ROOT / ".git").is_file():
        print("Run from an isolated worktree, not the shared main checkout.", file=sys.stderr)
        return 2
    fixture = ROOT / "target" / "background-cu-component-check"
    (fixture / "src").mkdir(parents=True, exist_ok=True)
    (fixture / "Cargo.toml").write_text('''[package]
name = "intendant-background-cu-component-check"
version = "0.0.0"
edition = "2021"
[workspace]
[dependencies]
core-graphics = { version = "0.25", features = ["highsierra"] }
core-foundation = "0.10"
accessibility-sys = "0.2"
libc = "0.2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }
''')
    cu = (ROOT / "src/bin/caller/computer_use.rs").read_text()
    observation = (ROOT / "src/bin/caller/cu_observation.rs").read_text()
    types = [
        '#[derive(Debug,Clone,serde::Serialize,serde::Deserialize,schemars::JsonSchema)]\n#[serde(tag="type",rename_all="snake_case")]\n' + item(cu, "CuAction"),
        '#[derive(Debug,Clone,Copy,Default,serde::Serialize,serde::Deserialize,schemars::JsonSchema)]\n' + item(cu, "MouseButton"),
        '#[derive(Debug,Clone,Copy,serde::Serialize,serde::Deserialize,schemars::JsonSchema)]\n' + item(cu, "ScrollDirection"),
        '#[derive(Debug,Clone,serde::Serialize)]\n' + item(cu, "UiElement"),
        '#[derive(Debug,Clone,serde::Serialize)]\n' + item(cu, "ScreenElements"),
        '#[derive(Debug,Clone,Copy,Default)]\n' + item(observation, "ObserveMode"),
    ]
    # The existing key parser is unchanged by this feature except visibility.
    # Its stub deliberately errors: tests cannot inject even a synthetic key.
    support = '''
fn default_scroll_amount() -> i32 { 3 }
pub const ELEMENT_TREE_MAX_DEPTH: usize = 12;
pub const ELEMENT_TREE_MAX_NODES: usize = 400;
pub fn cap_screen_elements_texts(_: &mut ScreenElements) {}
pub fn format_screen_elements(s: &ScreenElements) -> String { format!("{s:?}") }
pub mod macos_input {
    pub fn parse_key(_: &str) -> Result<(core_graphics::event::CGKeyCode,core_graphics::event::CGEventFlags),String> {
        Err("key parsing is outside this component test".into())
    }
}
'''
    paths = {
        "ax": ROOT / "src/bin/caller/ax.rs",
        "background_cu": ROOT / "src/bin/caller/background_cu.rs",
        "platform": ROOT / "crates/intendant-platform/src/platform/macos_process.rs",
    }
    source = '#![allow(dead_code)]\nmod computer_use {\n' + '\n'.join(types) + support + '\n}\n'
    source += '\n'.join(f'#[path = {json.dumps(str(path))}] mod {name};' for name, path in paths.items())
    source += '''
mod display { pub mod macos { pub mod background {
    pub async fn capture_window_png(_:u32,_:u32,_:u32)->Result<Vec<u8>,String> {
        Err("GUI capture is forbidden in this component test".into())
    }
}}}
'''
    (fixture / "src/lib.rs").write_text(source)
    command = ["cargo", "test", "--offline", "--manifest-path", str(fixture / "Cargo.toml"), "--", "--skip", "live_read_frontmost"]
    print("Checking production native modules; capture and MCP transport are outside this component harness.", flush=True)
    return subprocess.run(command, cwd=ROOT, env=os.environ.copy(), check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
