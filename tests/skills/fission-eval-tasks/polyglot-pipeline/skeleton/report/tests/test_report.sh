#!/usr/bin/env bash
# Starter test for report/report.sh (see report/SPEC.md).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPORT="$(dirname "$HERE")/report.sh"

TD=$(mktemp -d)
trap 'rm -rf "$TD"' EXIT

cat > "$TD/merged.jsonl" <<'EOF'
{"id":"a","name":"A","email":null,"amount":100,"date":"2025-01-01","tags":["vip","eu"]}
{"id":"b","name":"B","email":null,"amount":100,"date":"2025-01-02","tags":["eu"]}
{"id":"c","name":"C","email":null,"amount":50.5,"date":"2025-01-03","tags":[]}
{"id":"d","name":"D","email":null,"amount":75,"date":"2025-01-04","tags":["vip"]}
EOF

cat > "$TD/expected.json" <<'EOF'
{"count":4,"total_amount":325.5,"by_tag":{"eu":2,"vip":2},"top_spenders":[{"id":"a","amount":100},{"id":"b","amount":100},{"id":"d","amount":75}]}
EOF

bash "$REPORT" "$TD/merged.jsonl" > "$TD/got.json"

# Compare as canonicalized JSON (key order irrelevant).
python3 - "$TD/got.json" "$TD/expected.json" <<'PY'
import json, sys
def canon(p):
    return json.dumps(json.load(open(p)), sort_keys=True)
g, w = canon(sys.argv[1]), canon(sys.argv[2])
assert g == w, "report mismatch\nGOT:  %s\nWANT: %s" % (g, w)
PY

# Empty input case.
: > "$TD/empty.jsonl"
bash "$REPORT" "$TD/empty.jsonl" > "$TD/got_empty.json"
python3 - "$TD/got_empty.json" <<'PY'
import json, sys
g = json.load(open(sys.argv[1]))
assert g == {"count":0,"total_amount":0,"by_tag":{},"top_spenders":[]}, "empty mismatch: %r" % g
PY
echo "report starter test: OK"
