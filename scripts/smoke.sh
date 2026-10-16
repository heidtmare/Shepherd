#!/usr/bin/env bash
# End-to-end check against a running `docker compose up` stack:
# intake upload → storage → extraction → search, with events observed on the stream feed.
set -euo pipefail

STORAGE=${STORAGE_URL:-http://localhost:8081}
SEARCH=${SEARCH_URL:-http://localhost:8082}
INTAKE=${INTAKE_URL:-http://localhost:8083}
STREAM=${STREAM_URL:-http://localhost:8084}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
PDF=$ROOT/fixtures/multipage.pdf
CLAIM=$ROOT/fixtures/sample.claim.json

fail() { echo "FAIL: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || fail "$1 not installed"; }
need curl; need jq

search() { curl -fsS -X POST -H 'Content-Type: application/json' -d "$1" "$SEARCH/v1/search"; }
# Polls search with $1 until the document appears; prints the response.
wait_hit() {
  local res
  for _ in $(seq 1 75); do
    res=$(search "$1")
    if jq -e --arg id "$doc" '.hits | map(.doc_id) | index($id)' >/dev/null <<<"$res"; then echo "$res"; return 0; fi
    sleep 0.2
  done
  return 1
}

accepted=$(curl -fsS -X POST -H 'Content-Type: application/pdf' --data-binary @"$PDF" \
  "$INTAKE/v1/intake?filename=smoke-multipage.pdf")
doc=$(jq -r .document_id <<<"$accepted")
echo "document $doc"

start=$(date +%s)
res=$(wait_hit '{"q": "mold remediation", "include_pages": true, "limit": 100}') || fail "extracted text not searchable"
echo "extracted + indexed in ~$(( $(date +%s) - start ))s"
jq -c --arg id "$doc" '.hits[] | select(.doc_id == $id) | {highlight, matched_pages, schemas: .summary.schemas}' <<<"$res"

events=$(curl -sS -N --max-time 2 "$STREAM/v1/events?document_id=$doc" 2>/dev/null | sed -n 's/^data: //p' | jq -r .type) || true
echo "events:"; sed 's/^/  /' <<<"$events"
for t in storage.document.created intake.extraction.started storage.artifact.created intake.extraction.completed search.card.indexed; do
  grep -q "shepherd.$t" <<<"$events" || fail "missing event $t"
done

ir_id=$(curl -fsS "$STORAGE/v1/documents/$doc" | jq -r '.artifacts[] | select(.schema == "intake.ir.v1") | .id' | head -1)
curl -fsS "$STORAGE/v1/artifacts/$ir_id/content" | jq -c '{title, stats, needs_ocr: .flags.needs_ocr}'

# Simulated inference enrichment.
curl -fsS -X POST -H 'Content-Type: application/json' --data-binary @"$CLAIM" \
  "$STORAGE/v1/documents/$doc/artifacts?schema=inference.claim.v1&produced_by=smoke" >/dev/null
res=$(wait_hit '{"filters": [{"op": "eq", "field": "status", "value": "pending_review"}], "facets": ["type", "needs_ocr"], "limit": 100}') \
  || fail "enrichment not searchable"
jq -c '.facet_counts' <<<"$res"

want=$(shasum -a 256 "$PDF" | cut -d' ' -f1)
got=$(curl -fsS "$STORAGE/v1/documents/$doc/source" | shasum -a 256 | cut -d' ' -f1)
[[ "$want" == "$got" ]] || fail "source sha256 mismatch"
echo "source sha256 ok"

actions=$(curl -fsS "$STORAGE/v1/documents/$doc/audit" | jq -c 'map(.action + ":" + (.actor // "-"))')
echo "audit: $actions"

curl -fsS "$STREAM/v1/topology" | jq -c '.[] | {stream: .name, messages, consumers: [.consumers[] | {name, num_pending, num_ack_pending}]}'
echo PASS
