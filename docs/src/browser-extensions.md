# Browser extension approval

Browser workspaces accept no extensions by default. Approval belongs to the
owner starting the daemon, separately from a caller identifying an archive.
Intendant has no application or extension-vendor policy built into its binary.

Start the daemon with **both** explicit flags:

```sh
intendant --browser-extension-policy /absolute/approved-extensions.json \
  --browser-extension-policy-sha256 EXACT_RAW_FILE_SHA256
```

The owner reviews the archives and this file, then pins its exact raw SHA-256
(including whitespace) in trusted startup argv. Example schema (replace the
illustrative digest and length with the reviewed archive's real identity):

```json
{"schema_version":1,"extensions":[{"archive_sha256":"0000000000000000000000000000000000000000000000000000000000000000","archive_byte_length":1234,"manifest_version":3,"version":"1.0.0","service_worker":"worker.js"}]}
```

The daemon reads one bounded regular-file snapshot (1–65,536 bytes), without
following a symlink or Windows reparse leaf. It verifies the raw pin before
parsing, rejects duplicate/unknown/missing fields, and permits at most 32 distinct
archive identities. Digests are 64 lowercase hexadecimal characters; archives
are 1–64 MiB. Only MV3 is supported, with a canonical version of 1–4 integer
components (0–65535, no leading zeros, not all zero). Worker paths must be
literal portable relative paths, at most 512 bytes, without traversal, URL
escapes, Windows device names or filesystem aliases. The manifest must declare
that exact worker and version, and the worker must exist as a regular file.

Approval is initialized before project `.env` loading or any frontend/MCP
startup. There is no environment, project config, task, MCP, or settings method
that changes it. Editing the file cannot change the running policy. A successor
replays both startup flags and verifies the file again; changed bytes refuse
startup until the owner deliberately supplies a matching pin. Protect the argv
pin through the same trusted mechanism as the daemon launch. A caller-writable
file without that startup pin has no approval authority.

`ctl browser create` retains its existing all-or-none request tuple:
`--extension-archive`, `--extension-sha256`, `--extension-bytes`,
`--extension-manifest-version`, `--extension-version`. It must match a startup
approval exactly. Requests do not choose a policy or service-worker override.
Only `provider=cdp` with Intendant-managed Chrome for Testing and a fresh absolute
private profile is supported. No new shell or credential authority is granted.

The archive is snapshotted and verified once, safely extracted under private
daemon storage, and made read-only. Extraction still caps file count (4096),
individual unpacked entries (96 MiB), total unpacked bytes (256 MiB) and manifest
size (1 MiB). Traversal, links, special files and case-colliding paths fail closed.
Readiness requires the exact approved worker and one application page. All
extension targets, including onboarding and offscreen documents, must belong to
one runtime ID. That identity and entrypoint are checked again during CU
liveness probes. Existing cancellation, display-generation isolation and cleanup
lifetimes cover both the extension tree and profile.

The private workspace response adds `extension.service_worker`. Its value comes
from the matched approval and validated manifest. Old archived workspace records
can still deserialize without it; an empty entrypoint cannot authorize a live
extension. No session receipt profile, hash domain or digested field changes;
see [CU receipt compatibility](./external-cu-session.md#api-migration-and-receipt-compatibility).

To migrate, close existing attempts, review the desired byte-exact MV3 archives,
pin a policy at startup, and create fresh workspaces. Use `bounded_cu` leases and
`ctl cu session` for new external sessions. Keep existing receipts byte-for-byte
for replay; the deprecated API aliases do not accept old application lease kinds.

## Local acceptance

The Linux smoke generates two harmless MV3 fixtures with different workers
(`worker.js` and `background/observer.js`). It serves only a loopback page,
checks onboarding and offscreen message roundtrips, native 1024×768 viewport at
scale one, no-policy and foreign-extension rejection, immutable approval despite
file edits, and exact resource cleanup. It uses an already-installed local
Chrome for Testing runtime and Node 22+; it downloads nothing. All daemon state,
profiles and test cache references are private temporary resources.

```sh
python3 scripts/test-browser-extension.py --bin /absolute/intendant \
  --browser-dir /absolute/chrome-linux64 --node /absolute/node \
  --evidence /absolute/new-extension-evidence
python3 scripts/test-external-cu-session.py --bin /absolute/intendant \
  --browser-bin /absolute/chrome --evidence /absolute/new-cu-evidence
python3 scripts/check-browser-application-coupling.py --self-test
```

The real smokes require Linux/Xvfb/xauth and are intended for a validation host.
The coupling check and fixture discovery checks are small, build-free CI gates.
