# Handover lock-release investigation

Separate fix before re-queueing #936. Queue run 35343852948 failed dropped_runtime_releases_lease_and_liveness on macbook-b; the previous Linux PR run failed successor_port_never_names_a_draining_or_dead_target. Both handover files are unchanged by #936.

Investigating close-only lock ownership across duplicated/inherited descriptors. Preserve real lock exclusion and fail-closed probes; no sleeps, retries, test ignores, runner restarts or compile-governor changes.
