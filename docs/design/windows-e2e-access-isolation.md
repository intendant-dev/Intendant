# Windows E2E access-store isolation

The Windows access backend resolves RoamingAppData through a Known Folder, ignoring each fixture HOME/USERPROFILE. Parallel real-daemon E2E fixtures can share IAM, including the HTTP Tasks revocation fixture.

Fix explicit INTENDANT_HOME handling for Windows access state while preserving the legacy default when no override exists. Pin every TestRig child to its own state root. Keep IAM enforcement intact and add path plus cross-daemon regressions. Validation pending.
