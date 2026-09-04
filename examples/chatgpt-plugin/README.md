# Intendant ChatGPT plugin template

This directory contains the public, secret-free source for the personal
Intendant plugin package. It is deliberately a template: a registered MCP app
technical ID belongs to one ChatGPT/Platform setup and is added only when the
package is materialized.

See [the complete setup and security runbook](../../docs/src/chatgpt-plugin.md).

```bash
python3 examples/chatgpt-plugin/configure_plugin.py \
  --app-id plugin_asdk_app_0123456789abcdef0123456789abcdef \
  --output /absolute/path/to/intendant-plugin
```

The generated directory contains `.app.json`; the template does not. Never add
a runtime API key, Intendant loopback token, or live tunnel profile to either
directory.
