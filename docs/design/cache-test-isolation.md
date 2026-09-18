# Cache-test isolation after the Windows queue failure

On 2026-09-18, merge-group run 35347884241 (b1066bb0af3b37e61cac86fca2fe6bf3353e61c1) failed two Windows pointer-identity assertions: load_state_cached_matches_uncached_and_sees_external_writes and fleet_origin_provenance_cache_reuses_only_an_exact_file_generation. The macOS and Linux merge-group legs passed, including the handover fix.

Both caches are process-global, path-keyed maps with an eight-directory cap. Inserting a ninth distinct path clears the cache. Distinct temporary directories prevent content collisions, but do not prevent other tests from evicting an entry between reads.

The failure was reproduced without timing on macOS: fill each shared cache with eight other fixture roots between the existing first and second reads. Both original Arc::ptr_eq assertions fail (93 selected tests passed, 2 failed); reproduction.patch and red-cache.log are retained in target/windows-queue. The original Windows job did not trace individual evictions, so this proves the matching failure mechanism, not the exact historical thread order.

The same production read/store helpers now accept an explicit cache instance. Public entry points still pass their original global cache. The two identity tests pass private instances; their pointer, external-write, deletion, and file-generation checks remain intact.

Two additional tests deliberately exceed the per-instance capacity, assert eviction, verify unchanged file fingerprints and retained snapshot contents, then require a fresh object followed by a real cache hit. No sleeps, retries, global test serialization, skipped assertions, platform exclusions, cache-cap changes, or CI workflow changes are introduced.

The handover correction remains unchanged. IAM permissions, provenance fences, metadata failure behavior, cache invalidation, and public APIs remain unchanged. Windows runtime confirmation is provided by the full merge-group gate, not its compile-only PR leg.

Focused validation: 97 cache tests passed after the patch, followed by ten complete repetitions at 16 test threads, all passing (970 additional executions). Both original pointer-identity assertions remain. The full keyless battery and merge-group Windows runtime gate are tracked in the PR execution record.

Local validation passed: 6,553 binary tests (10 existing ignores), 1,022 library/acceptance tests (3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied, formatting and whitespace. Focused cache tests: 97 passed plus ten clean repetitions. Compile governor unchanged.
