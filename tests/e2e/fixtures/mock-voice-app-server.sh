#!/bin/sh
# Scripted mock Codex App Server for the composed voice-approval e2e
# (tests/e2e: voice_composed_approval_resolves_real_staged_approval_end_to_end).
#
# Spawned by the daemon's Voice broker through the real `[presence.voice]
# app_server_command` override seam, with the real hardened launch args
# (ignored here) and the real neutral cwd — this script's working
# directory is `<state root>/presence/neutral-cwd/`, where it appends
# every line it RECEIVES to `io.jsonl` for the test's wire assertions.
#
# Protocol: newline-delimited JSON-RPC on stdio, exactly what the broker
# speaks. The script answers the broker's requests with canned results,
# and plays the backing model's half of the R1 composed leg:
#
#   1. thread/realtime/start  -> reply {}, push the SDP answer + started.
#   2. The daemon stages a REAL approval mid-call; the broker's injection
#      lane delivers it as `thread/realtime/appendSpeech` whose text is
#      "Approval needed (id=N, ...)". The id is parsed FROM THAT SPEECH —
#      this script has no other input channel, so the approval id in the
#      tool call below can only have round-tripped through the daemon's
#      real staged approval.
#   3. First `item/tool/call approve_action` is sent BEFORE any spoken
#      evidence exists in the transcript -> the broker's R3 gate must
#      REFUSE it (refused-evidence-unmatched) and the approval must stay
#      pending.
#   4. A user-role `thread/realtime/transcript/done` is pushed (the owner
#      "speaks" the instruction), then the same tool call is repeated ->
#      the gate verifies the evidence and dispatches the real approval.
#   5. thread/realtime/stop -> reply {}, push closed.
#
# POSIX sh + sed only (the suite's shebang-exec fixture convention;
# the test is #[cfg(unix)] like the other shebang-fixture e2es).

IO_LOG="io.jsonl"
SPOKEN="approve the pending action right away"
approval_stage="waiting"   # waiting -> probed -> done

emit() {
    printf '%s\n' "$1"
}

reply() {
    # reply <id> <result-json>
    emit "{\"jsonrpc\":\"2.0\",\"id\":$1,\"result\":$2}"
}

while IFS= read -r line; do
    printf '%s\n' "$line" >> "$IO_LOG"
    id=$(printf '%s' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9][0-9]*\).*/\1/p')
    method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
    if [ -z "$method" ]; then
        # A response to one of this script's own server-requests
        # (the broker's tool-call verdicts land here) — logged above,
        # nothing to answer.
        continue
    fi
    case "$method" in
        initialize)
            reply "$id" '{"userAgent":"mock-voice-app-server/1.0"}'
            ;;
        experimentalFeature/list)
            reply "$id" '{"features":[{"name":"realtime_conversation","enabled":true}]}'
            ;;
        thread/start)
            reply "$id" '{"thread":{"id":"vt-e2e-1"},"model":"gpt-5-e2e","reasoningEffort":"medium"}'
            ;;
        thread/resume)
            reply "$id" '{"thread":{"id":"vt-e2e-1"},"model":"gpt-5-e2e","reasoningEffort":"medium"}'
            ;;
        thread/realtime/start)
            reply "$id" '{}'
            emit '{"jsonrpc":"2.0","method":"thread/realtime/sdp","params":{"threadId":"vt-e2e-1","sdp":"vpfix-answer-sdp"}}'
            emit '{"jsonrpc":"2.0","method":"thread/realtime/started","params":{"threadId":"vt-e2e-1","realtimeSessionId":"rs-e2e-1","version":"v3"}}'
            ;;
        thread/realtime/appendText|thread/realtime/appendSpeech)
            reply "$id" '{}'
            if [ "$approval_stage" = "waiting" ]; then
                approval_id=$(printf '%s' "$line" | sed -n 's/.*Approval needed (id=\([0-9][0-9]*\).*/\1/p')
                if [ -n "$approval_id" ]; then
                    approval_stage="probed"
                    # (3) Tool call WITHOUT spoken evidence in the
                    # transcript: the composed gate must refuse this.
                    emit "{\"jsonrpc\":\"2.0\",\"id\":9001,\"method\":\"item/tool/call\",\"params\":{\"threadId\":\"vt-e2e-1\",\"turnId\":\"turn-1\",\"callId\":\"call-refused\",\"tool\":\"approve_action\",\"arguments\":{\"id\":$approval_id,\"spoken_instruction\":\"$SPOKEN\"}}}"
                    sleep 1
                    # (4) The owner speaks, then the same call verifies.
                    emit "{\"jsonrpc\":\"2.0\",\"method\":\"thread/realtime/transcript/done\",\"params\":{\"threadId\":\"vt-e2e-1\",\"role\":\"user\",\"text\":\"Yes - $SPOKEN please.\"}}"
                    sleep 1
                    emit "{\"jsonrpc\":\"2.0\",\"id\":9002,\"method\":\"item/tool/call\",\"params\":{\"threadId\":\"vt-e2e-1\",\"turnId\":\"turn-1\",\"callId\":\"call-granted\",\"tool\":\"approve_action\",\"arguments\":{\"id\":$approval_id,\"spoken_instruction\":\"$SPOKEN\"}}}"
                    approval_stage="done"
                fi
            fi
            ;;
        thread/realtime/stop)
            reply "$id" '{}'
            emit '{"jsonrpc":"2.0","method":"thread/realtime/closed","params":{"threadId":"vt-e2e-1","reason":"requested"}}'
            ;;
        account/rateLimits/read)
            reply "$id" '{}'
            ;;
        thread/delete)
            reply "$id" '{}'
            ;;
        *)
            # Unknown request: answer {} so the broker never hangs;
            # notifications (no id) fall through silently.
            if [ -n "$id" ]; then
                reply "$id" '{}'
            fi
            ;;
    esac
done
