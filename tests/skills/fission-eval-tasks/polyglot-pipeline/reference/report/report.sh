#!/usr/bin/env bash
# Reference solution for the report component. See report/SPEC.md.
# Excluded from agent visibility by the SKILL runner.
set -euo pipefail

if [ "$#" -ne 1 ] || [ ! -r "$1" ]; then
  echo "usage: report.sh MERGED.jsonl" >&2
  exit 2
fi

# Slurp JSONL into an array and compute the report. total_amount is rounded to
# 2 decimals (multiply, round, divide); by_tag counts records per tag (tags are
# unique within a record); top_spenders is amount-desc then id-asc, capped at 3.
jq -s '
  . as $r
  | {
      count: ($r | length),
      total_amount: ((($r | map(.amount) | add) // 0) | (. * 100 | round) / 100),
      by_tag: ($r | reduce (.[].tags[]) as $t ({}; .[$t] = ((.[$t] // 0) + 1))),
      top_spenders: ($r | sort_by(.id) | sort_by(-.amount) | .[0:3] | map({id, amount}))
    }
' "$1"
