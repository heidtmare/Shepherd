#!/usr/bin/env bash
# Receipting check against a running compose stack: handwritten scan → recognition → task →
# resolve flagged fields via the API → receipt artifact → searchable.
set -euo pipefail

STORAGE=${STORAGE_URL:-http://localhost:8081}
SEARCH=${SEARCH_URL:-http://localhost:8082}
INTAKE=${INTAKE_URL:-http://localhost:8083}
ADJ=${WORKBENCH_URL:-http://localhost:8085}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SCAN=$ROOT/forms/samples/i-765.homemade.pdf
TRUTH=$ROOT/forms/samples/i-765.values.json
USER_NAME=${REVIEWER:-smoke}

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v jq >/dev/null || fail "jq not installed"
[[ -f "$SCAN" ]] || fail "missing $SCAN (cargo run --release -p shepherd-forms -- synth ...)"

doc=$(curl -fsS -X POST -H 'Content-Type: application/pdf' --data-binary @"$SCAN" \
  "$INTAKE/v1/intake?filename=smoke-i765.pdf" | jq -r .document_id)
echo "document $doc"

task=""
for _ in $(seq 1 240); do
  task=$(curl -fsS "$ADJ/v1/documents/$doc/tasks" | jq -r '[.[] | select(.status == "open")][0].id // empty')
  [[ -n "$task" ]] && break
  sleep 0.5
done
[[ -n "$task" ]] || fail "no receipting task created"
detail=$(curl -fsS "$ADJ/v1/tasks/$task")
jq -c '{form: .task.form_type, flagged: .task.flagged_fields, total: .task.total_fields, pages: (.pages | length)}' <<<"$detail"
echo "flagged:"; jq -r '.recognition.fields[] | select(.status == "requires_receipting") | "  \(.id | split(".") | last)  read=\(.raw // "-")  \([.reasons[].code] | join(","))"' <<<"$detail"

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "X-User: $USER_NAME" -H 'Content-Type: application/json' \
  -d '{"mode":"review","values":{}}' "$ADJ/v1/tasks/$task/submit")
[[ "$code" == 422 ]] || fail "unresolved submit returned $code, expected 422"

# Resolve every flagged field with the ground truth (a reviewer typing what is on the page).
values=$(jq -c --slurpfile t "$TRUTH" '[.recognition.fields[] | select(.status == "requires_receipting") | {key: .id, value: ($t[0][.id] // null)}] | from_entries' <<<"$detail")
done=$(curl -fsS -X POST -H "X-User: $USER_NAME" -H 'Content-Type: application/json' \
  -d "{\"mode\":\"review\",\"values\":$values}" "$ADJ/v1/tasks/$task/submit")
jq -c '{status: .task.status, stats}' <<<"$done"
receipt=$(curl -fsS "$STORAGE/v1/artifacts/$(jq -r .receipt_artifact_id <<<"$done")/content")

mismatch=$(jq -r --slurpfile t "$TRUTH" '
  def norm: ascii_downcase | gsub("[^a-z0-9]"; "");
  [$t[0] | to_entries[] | select((.value | norm) != (($receipt.values[.key] // "") | norm)) | .key] | length' \
  --argjson receipt "$receipt" <<<'{}')
[[ "$mismatch" == 0 ]] || fail "$mismatch receipted value(s) differ from ground truth"
echo "receipt values match ground truth"

for _ in $(seq 1 50); do
  hit=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    -d '{"filters":[{"op":"facet","value":"receipted/review"}],"limit":100}' "$SEARCH/v1/search" \
    | jq --arg id "$doc" '[.hits[].doc_id] | index($id)')
  [[ "$hit" != null ]] && break
  sleep 0.2
done
[[ "$hit" != null ]] || fail "receipt not searchable"
echo "searchable via facet receipted/review"
echo "workbench: $ADJ/#/tasks/$task"
echo PASS
