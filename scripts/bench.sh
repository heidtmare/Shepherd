#!/usr/bin/env bash
# Latency check for /v1/search using oha (https://github.com/hatoo/oha).
set -euo pipefail
SEARCH=${SEARCH_URL:-http://localhost:8082}
DURATION=${DURATION:-20s}
CONC=${CONCURRENCY:-16}
command -v oha >/dev/null || { echo "install oha: cargo install oha" >&2; exit 1; }

run() {
  echo "== $1"
  oha -z "$DURATION" -c "$CONC" --no-tui -m POST -H 'Content-Type: application/json' -d "$2" "$SEARCH/v1/search" \
    | grep -E 'Requests/sec|Slowest|Fastest|Average|99.00%|Status code distribution' -A0
}

run "keyword"          '{"q": "hail shingles"}'
run "keyword+filter"   '{"q": "water damage", "filters": [{"op": "eq", "field": "status", "value": "approved"}, {"op": "range", "field": "amount", "gte": 1000, "lt": 5000}]}'
run "keyword+facets"   '{"q": "collision bumper", "facets": ["type", "status"]}'
run "browse sorted"    '{"sort": {"field": "updated_at"}, "filters": [{"op": "facet", "value": "type/fire"}]}'
run "fuzzy filename"   '{"q": "bench-4242.pdf"}'
