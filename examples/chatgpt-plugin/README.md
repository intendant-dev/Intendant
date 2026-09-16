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

The default package contains **no dogfooding skill**. Developer feedback is
opt-in, not an end-user behavior.

For a developer-only package, add `--dev-dogfood` to the command above. This
copies `skills-internal/skill-dogfood-feedback/SKILL.md` into the generated
package, never into the default template. Separately start the developer daemon
with `INTENDANT_DEV_DOGFOOD=1` and use a caller authorized for `feedback.write`.
The package flag neither enables the daemon nor grants permission. Ordinary
`role:operator` does not grant feedback authority.

To retire an older package that bundled dogfooding by default, regenerate into
a new directory without the flag and reinstall it. The updated daemon refuses
stale `report` calls while developer dogfooding is disabled, even from root.
Existing reports remain local; nothing is sent centrally.

Secret-free generator tests:

```bash
python3 -m unittest scripts/test_configure_intendant_plugin.py
```
