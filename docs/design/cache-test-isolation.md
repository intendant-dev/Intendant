# Cache-test isolation after the Windows queue failure

On 2026-09-18, merge-group run 35347884241 (b1066bb0af3b37e61cac86fca2fe6bf3353e61c1) failed two Windows pointer-identity assertions: load_state_cached_matches_uncached_and_sees_external_writes and fleet_origin_provenance_cache_reuses_only_an_exact_file_generation. The macOS and Linux merge-group legs passed, including the handover fix.

Both caches are process-global, path-keyed maps with an eight-directory cap. Inserting a ninth distinct path clears the cache. Distinct temporary directories prevent content collisions, but do not prevent other tests from evicting an entry between reads.

The failure was reproduced without timing on macOS: fill each shared cache with eight other fixture roots between the existing first and second reads. Both original Arc::ptr_eq assertions fail (93 selected tests passed, 2 failed); reproduction.patch and red-cache.log are retained in target/windows-queue. The original Windows job did not trace individual evictions, so this proves the matching failure mechanism, not the exact historical thread order.

The same production read/store helpers now accept an explicit cache instance. Public entry points still pass their original global cache. The two identity tests pass private instances; their pointer, external-write, deletion, and file-generation checks remain intact.

Two additional tests deliberately exceed the per-instance capacity, assert eviction, verify unchanged file fingerprints and retained snapshot contents, then require a fresh object followed by a real cache hit. No sleeps, retries, global test serialization, skipped assertions, platform exclusions, cache-cap changes, or CI workflow changes are introduced.

The handover correction remains unchanged. IAM permissions, provenance fences, metadata failure behavior, cache invalidation, and public APIs remain unchanged. Windows runtime confirmation is provided by the full merge-group gate, not its compile-only PR leg.

Focused validation: 97 cache tests passed after the patch, followed by ten complete repetitions at 16 test threads, all passing (970 additional executions). Both original pointer-identity assertions remain. The full keyless battery and merge-group Windows runtime gate are tracked in the PR execution record.

Local validation passed: 6,553 binary tests (10 existing ignores), 1,022 library/acceptance tests (3 existing ignores), 57 E2E tests, workspace Clippy with warnings denied, formatting and whitespace. Focused cache tests: 97 passed plus ten clean repetitions. Compile governor unchanged.

## Subsequent Linux fixture failure

The next merge-group run, 35351117489, passed the complete Windows and macOS jobs. Linux instead failed codex_cloud::tests::submit_prompt_uses_stdin_instead_of_process_arguments before fake-codex could start: Text file busy (ETXTBSY, errno 26). Both repaired cache tests passed on Windows.

The fake CLI was written by the multithreaded test process. A concurrent fork can inherit the writable executable descriptor before its close, which Linux refuses to execute while any writer survives. A controlled Linux-container fork/pipe experiment reproduced errno 26 while the child retained that descriptor; execution succeeded after the child exited.

All seven fake-CLI creation sites in the codex_cloud test module now use a dedicated, waited-for shell child to write and chmod the file. The parent never opens that executable for writing; unrelated test forks cannot inherit its writer. Path and contents are quoted positional parameters, not interpolated shell source. A new test pins literal bytes, quoted/spaced paths, executable mode, argument delivery, and write-error reporting.

The production codex_cloud code is byte-identical, including prompt-via-stdin behavior. Existing secret/argv assertions remain unchanged. No exec retries, sleeps, test serialization or ignores are added. The original CI job lacked descriptor tracing, so the precise historical child interleaving remains unobserved.

Final local validation including the Linux fixture change: 6,554 binary tests (10 existing ignores), 1,022 required library/acceptance tests (3 existing ignores), 57 E2E; Clippy, formatting and whitespace passed. CLI focused suite: 69 passed plus five clean repetitions.
