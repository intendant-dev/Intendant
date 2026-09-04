# ChatGPT plugin over Secure MCP Tunnel

Intendant can be used from ChatGPT and Codex through an OpenAI Secure MCP
Tunnel without making the daemon reachable from the public internet. The
tracked integration kit keeps the reusable pieces in source control and leaves
every credential and account-specific registration outside the repository.

```text
ChatGPT / Codex
      |
      v
OpenAI-hosted tunnel endpoint
      ^  outbound HTTPS
      |
tunnel-client -> 127.0.0.1:18766/mcp
                         |
                         v
        intendant-mcp-relay.py
                         |
             active port + per-boot
             loopback admission token
                         |
                         v
              Intendant daemon /mcp
```

OpenAI documents Secure MCP Tunnel as an outbound-only path for private MCP
servers. It is suitable for private and developer-mode connections, but it is
not itself a public-plugin distribution endpoint. A plugin submitted to the
public directory needs a stable publicly reachable HTTPS MCP endpoint. See the
[Secure MCP Tunnel guide](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
and [plugin packaging guide](https://developers.openai.com/plugins/build/plugins).

## What is tracked

| Part | Location | Why |
|---|---|---|
| Stable loopback relay | `scripts/intendant-mcp-relay.py` | Follows `cli-path.meta.json` across daemon port handovers and injects the current admission token without logging it. |
| Relay tests | `scripts/test_intendant_mcp_relay.py` | Covers handover, concurrent requests, header isolation, and opaque failures. |
| Plugin source template | `examples/chatgpt-plugin/plugin-template/` | Canonical manifest and public branding, with no registered-app binding. |
| Per-install generator | `examples/chatgpt-plugin/configure_plugin.py` | Adds the non-secret `plugin_asdk_app...` technical ID to a generated package. |
| Supervisor examples | `examples/chatgpt-plugin/services/` | Secret-free launchd and systemd user-service examples for the relay and tunnel client. |

The 128px composer icon is under 10 KiB. The larger square logo and wide banner
are separate assets for richer plugin surfaces.

## What must remain untracked

Do not commit any of these:

- a runtime API key or a tunnel-client profile that refers to one;
- files under the Intendant state root, especially `loopback-tokens/`;
- a live `cli-path.meta.json`, tunnel-client health file, logs, or downloaded
  binary;
- the installed plugin cache or personal marketplace state;
- a generated `.app.json`, unless a project deliberately wants to publish a
  deployment binding rather than this reusable template;
- a machine-rendered launchd/systemd file containing user-specific absolute
  paths.

Tunnel IDs and `plugin_asdk_app...` IDs are identifiers rather than bearer
secrets, but keeping the live values out of the public template avoids coupling
the repository to one account, workspace, or private tunnel.

## Set up a private connection

Prerequisites are a running Intendant daemon with its HTTP `/mcp` surface, an
OpenAI tunnel for the intended Platform organization/ChatGPT workspace, the
public `tunnel-client` binary, and ChatGPT developer mode. Keep the tunnel
client's runtime credential in an environment or secret manager outside the
checkout.

1. Start the stable relay:

   ```bash
   python3 scripts/intendant-mcp-relay.py
   ```

   It listens only on `127.0.0.1:18766` by default. Use `--state-root` when the
   daemon uses a non-default `INTENDANT_HOME`.

2. Initialize the private tunnel-client profile using OpenAI's CLI flow. Point
   its HTTP MCP server URL at the relay:

   ```bash
   tunnel-client init \
     --profile intendant \
     --tunnel-id tunnel_REPLACE_WITH_YOUR_PRIVATE_TUNNEL_ID \
     --mcp-server-url http://127.0.0.1:18766/mcp

   tunnel-client doctor --profile intendant --explain
   tunnel-client run --profile intendant
   ```

   The profile and runtime credential stay outside Git. Do not put the key on a
   command line, in a service template, or in this repository.

3. In ChatGPT developer mode, create an MCP app using **Tunnel** transport and
   select the intended tunnel. Copy the resulting technical ID from the app URL;
   it starts with `plugin_asdk_app_`.

4. Materialize the plugin package in an untracked personal location:

   ```bash
   python3 examples/chatgpt-plugin/configure_plugin.py \
     --app-id plugin_asdk_app_0123456789abcdef0123456789abcdef \
     --output /absolute/path/to/personal/intendant-plugin
   ```

5. Validate and install that generated directory with Plugin Creator, or add it
   to a personal marketplace. Enable it for the intended ChatGPT/Codex surface
   and test from a new chat.

For unattended use, copy the relevant service examples, replace every absolute
path placeholder, validate the rendered service, and install it as a user
service. Run the relay and tunnel client as the same OS account that owns the
Intendant state root. Foreground commands work on every supported platform even
when no native supervisor example is provided here.

## Security properties

The relay is intentionally narrow:

- it binds only to a loopback IP and accepts only `/mcp`;
- it resolves the active daemon descriptor for each request, so a daemon update
  can change the web port without editing the tunnel profile;
- it reads the per-port admission token only at forwarding time, overwrites any
  caller-supplied Intendant token, strips ambient authorization/cookie headers,
  and never logs request headers;
- failures return only `relay unavailable`, never a path, token, or exception;
- it creates one upstream connection per request and uses a threaded loopback
  server, so multiple MCP consumers can make progress concurrently. Individual
  Intendant tools can still serialize internally when their resource or control
  plane requires it.

The tunnel does not weaken Intendant's call-time MCP authorization. The relay
authenticates as the local-process principal; configure Intendant IAM for that
principal to the least role the private plugin needs.

## Verification and troubleshooting

Run the secret-free local tests:

```bash
python3 -m unittest scripts/test_intendant_mcp_relay.py

temporary_plugin="$(mktemp -d)/intendant"
python3 examples/chatgpt-plugin/configure_plugin.py \
  --app-id plugin_asdk_app_0123456789abcdef0123456789abcdef \
  --output "$temporary_plugin"
python3 /path/to/plugin-creator/scripts/validate_plugin.py "$temporary_plugin"
```

For a live connection, check in this order:

1. Intendant's `cli-path.meta.json` exists and names the currently running
   gateway port.
2. The relay process is listening on `127.0.0.1:18766`.
3. `tunnel-client doctor --profile intendant --explain` reports the local MCP
   endpoint ready.
4. The tunnel client is running and connected.
5. The ChatGPT app uses Tunnel transport and the intended tunnel.
6. A new chat can list the Intendant MCP tools and call a read-only tool.

An HTTP 502 from the plugin usually means the relay could not reach the active
daemon, could not read the matching local admission token, or the tunnel client
was running while Intendant was between handover states. The relay follows a
completed handover automatically; persistent 502s should be diagnosed at the
first failing layer above.
