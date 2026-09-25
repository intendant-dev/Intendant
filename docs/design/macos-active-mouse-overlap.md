# Native mouse-activity overlap study (2026-09-24)

Fixture-only follow-up to #954. Require observed HID mouse-motion counter progress while one existing production ArrowLeft or ArrowRight dispatch call is in flight. The native fixture remains observation-only: no mouse/key posting, activation, focus mutation, event taps, input contents, or user-app inspection. The production arrow path, IAM, exact retained receiver, protection/focus checks, 50 ms AX timeout, four-second operation budget, and one-use semantics are unchanged.

The fixed acceptance uses a fresh disposable Chrome for Testing profile per direction on an owned virtual monitor, with the existing separately verified setup click. A dispatch bracket starts before the production press request. The harness waits up to a bounded interval for passive mouse-motion evidence, then sends the single production arrow while continuing to sample the same bracket. Acceptance requires mouse-motion counter progress both before the request and again during the client request interval, exactly one trusted arrow pair/effect in the synthetic field, no observed keyboard HID activity, unchanged foreground-focus endpoints, no target foreground observation, and exact cleanup. Counter evidence is not authenticated human identity and does not prove overlap with the narrower native posting instant.

No retry-until-success: failure to observe the required activity, any production refusal/uncertainty, malformed evidence, changed receiver/geometry, unexpected key content, or cleanup failure ends that fresh run. Separate ArrowLeft and ArrowRight runs are predetermined tests, not retries. This study must not weaken a production refusal or claim continuous isolation.

## Request accounting correction (2026-09-25)

The passive mouse gate now runs inside the dispatch witness interval but before the request-attempt checkpoint. A gate refusal or expiry records `dispatch_request_attempted: false` and no client-request duration; both end-witness and available fixture-state observations remain preserved. A transport exception after launch still records an attempted request and never triggers input replay. The original eight-second activity wait and all input/focus/protection checks are unchanged. Older reports are retained verbatim: their `attempted` flag marked entry into the wrapper, which could still be waiting for mouse activity, not necessarily a launched request.

## Witness bracket versus live request (2026-09-25)

New reports separate activity over the whole dispatch witness bracket from activity
observed while the dispatch client process is actually alive. A passive-gate
refusal can therefore report witness-bracket activity while
dispatch_request_attempted is false and both live-client activity fields remain
unknown. When a client is launched, dispatch_client_activity is checkpointed
before mouse-only overlap validation, so a later evidence refusal does not erase
whether activity progressed while that client was alive. These counters remain
unauthenticated source context and do not prove overlap with the narrower native
posting instant or continuous isolation. Historical reports are retained as
written and are not reinterpreted under the new field names.
