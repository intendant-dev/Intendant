# Windows E2E access-store isolation

The Windows access backend previously resolved RoamingAppData through a Known
Folder, ignoring each fixture's HOME/USERPROFILE. Parallel real-daemon E2E
fixtures could share IAM, including the HTTP Tasks revocation fixture.

The backend now uses `<INTENDANT_HOME>/access-certs` when the override is
explicit and nonempty. `intendant-core::state_paths::intendant_home_override`
caches both presence and the resolved path once per process, and
`intendant_home` uses that same resolution. Absolute roots pass through;
relative roots resolve against the directory at first use. Later environment
or working-directory changes cannot split access state from other daemon state.

Unset or empty `INTENDANT_HOME` preserves the Windows default exactly:
`dirs::data_dir()/intendant/access-certs` (RoamingAppData), or
`temp_dir()/intendant-access-certs` if the Known Folder is unavailable. There
is no migration, copying, deletion, or reset of existing user state. IAM
permissions and enforcement are unchanged.

`TestRig::command` pins every Intendant child to
`home/.intendant`, overriding any inherited `INTENDANT_HOME`. The daemonless
coordination helper retains that pin; the separate CLI-resolution shell also
receives it. Agenda item-id assertions include exit status, stdout, and stderr
so a structured refusal cannot collapse into an unexplained missing-id panic.

Regression coverage:

- Pure input-parameter unit tests cover absolute/relative overrides, unset and
  empty values, Windows override precedence, RoamingAppData, and the legacy
  temp fallback. They do not mutate the process environment or working directory.
- `http_tasks_revocation_denies_session_notifications_on_the_wire` now boots
  two real mock-provider daemons in distinct roots and creates the identical
  loopback grant on both. It verifies separate `access-certs/iam.json` files,
  revokes A, and retains all existing denial/restoration assertions. While A
  is revoked, B's existing Tasks session and a fresh ctl agenda write still
  succeed, and B's persisted IAM bytes remain unchanged. No retry or test
  suppression is added.

Local validation passed: 6,481 binary tests (10 existing ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied, formatting and whitespace. The two-daemon revocation regression passed separately. Compile governor unchanged. Windows CI remains required.

Review also found that Windows drive-relative paths were not fully pinned. Explicit relative roots now resolve through std::path::absolute before caching; invalid resolution is a fatal configuration error (status 2), never a fallback to the default IAM store. Pure negative tests and Windows-specific path regressions were added.
