# Intendant

A Rust runtime for autonomous AI agents with process lifecycle management. Intendant executes commands on behalf of AI agents, tracks process state in memory, and persists structured session logs. The CLI supports OpenAI, Anthropic, and Gemini APIs with native tool calling, a ratatui TUI, configurable autonomy, MCP server/client, and multi-agent orchestration.

## Architecture

```
                          ┌─────────────────────────────┐
                          │     intendant (caller)       │
                          │                             │
  TUI / MCP / Live ◄─────┤  presence ── agent loop ──┐ │
                          │     │           │         │ │
                          │     │      ┌────┴────┐    │ │
                          │     │      │ sub-agents│   │ │
                          │     │      └─────────┘    │ │
                          └─────┼─────────────────────┼─┘
                                │                     │
                                v                     v
                          Model APIs           intendant-runtime
                     (OpenAI/Anthropic/        (sequential command
                      Gemini + streaming)       execution, stdin/stdout)
```

**Presence layer** mediates between the user and agent loop — handles conversation, dispatches tasks, narrates events.
Three execution modes: *direct* (single agent), *user* (orchestrator + sub-agents), *sub-agent* (scoped child task).

## Quick Start

```bash
# Build
cargo build --release

# Install (optional)
cargo install --path .

# Set up API keys (~/.config/intendant/.env for global use)
echo 'OPENAI_API_KEY=sk-...' > .env

# Run with TUI
./target/release/intendant "List the files in /tmp"

# Headless mode
./target/release/intendant --no-tui "echo hello"

# Choose provider/model
./target/release/intendant --provider anthropic --model claude-sonnet-4-5-20250929 "Fix the tests"

# Run as MCP server
./target/release/intendant --mcp "Deploy the application"

# Live gateway (voice/text from browser)
./target/release/intendant --live
```

## Testing

```bash
cargo test
```

## Documentation

**[Read the full documentation](https://lovon-spec.github.io/intendant/)** — covers configuration, runtime protocol, TUI & autonomy, MCP server, integrations, and session logging.

Or build locally with [mdBook](https://rust-lang.github.io/mdBook/):

```bash
mdbook serve docs
```
