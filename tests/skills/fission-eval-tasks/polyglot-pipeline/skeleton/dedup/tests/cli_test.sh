#!/usr/bin/env bash
# Starter test for the dedup binary (see dedup/SPEC.md).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DEDUP_DIR=$(dirname "$HERE")
(cd "$DEDUP_DIR" && cargo build --release --quiet)
BIN="$DEDUP_DIR/target/release/dedup"

TD=$(mktemp -d)
trap 'rm -rf "$TD"' EXIT

cat > "$TD/a.jsonl" <<'EOF'
{"id":"x1","name":"A","email":null,"amount":10,"date":"2025-01-01","tags":["t1"]}
{"id":"y2","name":"B","email":"b@e.com","amount":5.5,"date":"2025-03-01","tags":["a","z"]}
EOF
cat > "$TD/b.jsonl" <<'EOF'
{"id":"x1","name":"A2","email":"a2@e.com","amount":20,"date":"2025-02-01","tags":["t2"]}
{"id":"x1","name":"A3","email":null,"amount":30,"date":"2025-02-01","tags":["t1","t3"]}
{"id":"w0","name":"W","email":null,"amount":1,"date":"2024-12-31","tags":[]}
EOF
# x1: newest date 2025-02-01 ties between A2 and A3 -> A3 (later position) wins;
# tags = union over the whole group. Output sorted by id.
cat > "$TD/expected.jsonl" <<'EOF'
{"id":"w0","name":"W","email":null,"amount":1,"date":"2024-12-31","tags":[]}
{"id":"x1","name":"A3","email":null,"amount":30,"date":"2025-02-01","tags":["t1","t2","t3"]}
{"id":"y2","name":"B","email":"b@e.com","amount":5.5,"date":"2025-03-01","tags":["a","z"]}
EOF

"$BIN" "$TD/a.jsonl" "$TD/b.jsonl" > "$TD/got.jsonl"

python3 - "$TD/got.jsonl" "$TD/expected.jsonl" <<'PY'
import json, sys
got = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
want = [json.loads(l) for l in open(sys.argv[2]) if l.strip()]
assert got == want, "dedup output mismatch\nGOT:  %r\nWANT: %r" % (got, want)
PY
echo "dedup starter test: OK"
