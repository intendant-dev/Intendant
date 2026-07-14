# Security Policy

Intendant is **alpha software** that operates real machines: it runs an AI
agent with a shell, a desktop it can see and control, credentials it can
borrow, and federation to peer machines. Security reports are taken
seriously and handled with priority.

## Reporting a vulnerability

**Please do not open a public issue for an exploitable bug.**

Report privately via GitHub's security advisory flow:
**[Security → Report a vulnerability](https://github.com/intendant-dev/Intendant/security/advisories/new)**
on this repository. You'll get an acknowledgment there; expect
alpha-stage response times (days, not hours) and no bug bounty — credit
in the advisory and release notes is gladly given.

If the advisory form is unavailable, open a plain issue that says only
"security report — requesting a private channel" with **no technical
detail**, and a maintainer will arrange one.

## Scope

Highest-value reports, roughly in order:

- **The runtime/controller boundary** — anything that lets a compromised
  model conversation reach API keys, or the sandboxed runtime reach model
  APIs (`intendant-runtime` is Landlock/Seatbelt/restricted-token
  sandboxed and must never hold keys).
- **Trust architecture / IAM** — minting or exercising daemon authority
  without a trusted anchor: hosted/Connect provenance escaping its
  immutable `role:none`, fleet-name (discovery-only) origins reaching
  control surfaces, grant/role/ceiling bypasses, org-document or
  revocation-list forgery.
- **Credential custody** — vault unsealing without the owner, lease
  escalation or exfiltration, leaking materialized OAuth files beyond
  their documented lifetime.
- **Gateway authentication** — mTLS/loopback/peer-identity bypasses,
  origin-gate (CSRF/DNS-rebind) escapes, authority surviving revocation.
- **Sandbox escapes** on any platform, and **federation** trust bypasses
  between paired daemons.
- The **release channel**: installer or update-path integrity, artifact
  substitution the transparency log would not catch.

Out of scope: vulnerabilities in the model providers themselves, social
engineering of the operator, and reports that require an already-root
local attacker.

## Supported versions

Alpha: only the **latest tagged release** and current `main` receive
fixes. There are no backports.

## Honest limits

The documented trust model states its own boundaries — see
[Trust Architecture](docs/src/trust-architecture.md) and
[Credential Custody](docs/src/credential-custody.md) for what the system
does *not* defend against (e.g., the hosted rendezvous is trusted for
availability and for the browser code and installers it serves). Reports
that sharpen or contradict those stated limits are welcome too.
