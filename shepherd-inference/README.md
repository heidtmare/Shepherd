# shepherd-inference

Rules evaluated on benefit cases as their data changes. Findings are stored for review, published as events, and written to the case's primary document.

- Input: durable consumer `inference-rules` on `shepherd.storage.artifact.created.*`. Only `workbench.case.v1` (the case projection written by shepherd-workbench after each change to case facts, status or documents) is read; other schemas are acked unread. A projection that is no longer its document's latest is skipped; the newer one's event follows.
- Output: `inference.findings.v1` on the case's primary document. When workbench retracts the case projection from a former primary (`retracted: true`), findings are rewritten on the new primary and the former primary's findings artifact is replaced by an empty retraction. Events: `shepherd.inference.finding.{raised,resolved}`, with the primary document as the subject. Payloads carry ids, rule and severity, not party data.

## Evaluation

Each projection replaces the case's parties (applicant, and beneficiary where the benefit has one: name, A-Number, date of birth) in the identity index (`inference.parties`). The case is then evaluated, together with every case whose findings can change with it:

- cases sharing an A-Number with it, before or after the change;
- cases with an open finding naming it.

These other cases are re-evaluated only when a value their rules see changed: the parties, receipt number or form type. Status, due dates and documents do not reach them.

An A-Number held by more than `A_NUMBER_CASE_LIMIT` cases is not joined: parties of other cases holding it are not loaded, and changes to it are not fanned out to its other holders unless it crosses the limit. `a_number_widely_shared` reports it on each holder.

An evaluation holds Postgres advisory locks on the cases it evaluates and their A-Numbers, taken in key order. The lock set is read, locked, and read again under the locks; if it grew, the transaction restarts with the union. Evaluations of unlinked cases run concurrently. Projections older than the stored revision are ignored, so redelivery and DLQ replay are safe.

Rules return detections, each identified within the case by a key. Against the case's open findings:

| Detection | Result |
|---|---|
| New `(rule, key)` | Finding raised (`finding.raised`). |
| Existing, evidence changed | Finding updated. A change in the related cases clears its review. |
| No longer detected | Finding resolved (`finding.resolved`). Detected again later: a new finding is raised. |

Findings are recomputed from the rules on every evaluation. A disabled or removed rule's findings resolve.

## Rules

Configured in `config/rules.toml` (`RULES_PATH`): one table per rule id with `enabled`, `severity` (`low`, `medium`, `high`) and the rule's parameters. Unknown rule ids and parameters fail at startup. Rules not listed are off. At startup, if the configuration or a rule version differs from the one last applied, every case is re-evaluated in the background.

| Rule | Detects | Key | Parameters |
|---|---|---|---|
| `a_number_name_mismatch` | An A-Number of the case recorded for a person whose name does not match: on another case, or for the other party of the same case (e.g. I-130 petitioner and beneficiary). A-Numbers over the case limit are not compared. | A-Number | `edit_ratio` (0.25) |
| `a_number_widely_shared` | An A-Number of the case held by more than `A_NUMBER_CASE_LIMIT` cases (placeholder, recurring misread, shared identity). Evidence: `a_number`, `case_limit`, `holders` (the case's own parties). No related cases. | A-Number | |

Name matching (`src/names.rs`): names are upper-cased, Latin diacritics folded, apostrophes and periods dropped, and split into tokens on spaces and hyphens. Two names match when:

- the tokens are equal in any order (given and family name swapped), or equal once joined (`DE LA CRUZ` / `DELACRUZ`);
- every token of the shorter name matches a distinct token of the other: exactly, as an initial, or within `floor(len × edit_ratio)` edits (insertion, deletion, substitution, adjacent transposition). This covers a missing middle name or second surname, typos and handwriting-recognition errors.

A party with no name, or no A-Number, is not compared. Name changes (e.g. after marriage) are reported; the reviewer dismisses them.

Evidence of an `a_number_name_mismatch` finding: `a_number`, `holders` (the case's parties with it), `conflicting` (parties whose names do not match, with `conflicts_with`: the holder roles), `consistent` (parties on other cases whose names match). Each party lists case id, receipt number, form type, role, names and date of birth. Case status is left out, so a status change does not rewrite other cases' findings.

Case facts are derived from the recognition until the form is receipted, so a misread A-Number or name can raise a finding that resolves once receipting corrects it.

### Adding a rule

1. Implement `rules::Rule` in a module under `src/rules/`: a pure function of `Context` (the case, its parties, the parties of other cases sharing an A-Number, and the case's A-Numbers over the case limit) returning `Detection`s with a stable `key`, a summary, evidence and related case ids. Evidence should hold only values whose change re-evaluates the case (not other cases' status).
2. Add its `RuleDef` (id, description, version, parameter builder) to `rules::CATALOG`. Parameters deserialize with `deny_unknown_fields`.
3. Configure it in `config/rules.toml`.
4. Bump `RuleDef::version` when its logic changes, so existing cases are re-evaluated.

Rules that match on another identifier (name and date of birth, address) need that join in `db::related` and `db::affected`, and its keys in the engine's lock set (`Engine::needs`).

## Review

`POST /v1/findings/{id}/review` records `confirmed` or `dismissed` (note required) on an open finding, or clears the review (`null`). Dismissed findings stay open but are left out of `inference.findings.v1`. When the related cases of a finding change, its review is cleared.

`inference.findings.v1`:

```json
{
  "schema": "inference.findings.v1", "case_id": "…", "receipt_number": "SHP2600000042",
  "case_artifact_id": "…", "revision": 3, "dismissed": 0,
  "findings": [{ "id": "…", "rule": "a_number_name_mismatch", "key": "012345678", "severity": "high",
                 "summary": "A-Number A012345678 of the applicant Nguyen, Linh is also recorded for Okafor, Chinedu (SHP2600000007 applicant)",
                 "evidence": { "…": "…" }, "related_cases": ["…"], "review": null, "raised_at": "…" }]
}
```

Written after each change to the case's open findings or reviews; a loop every `SYNC_SECS` retries failed writes. shepherd-search indexes it as the facets `finding` (rule ids) and `finding_severity`.

## API

| Method | Path | |
|---|---|---|
| GET | `/v1/rules` | Rules with severity, parameters, version; configuration fingerprint. |
| GET | `/v1/findings?case_id=&rule=&status=open\|resolved\|all&review=none\|confirmed\|dismissed&severity=&limit=` | Newest first. Default `status=open`. |
| GET | `/v1/findings/{id}` | |
| POST | `/v1/findings/{id}/review` `{disposition, note?}` | `X-User` required. Open findings only; 409 otherwise. |
| GET | `/healthz`, `/readyz`, `/metrics`, `/openapi.json` | |

No authentication; deploy behind an auth proxy that sets `X-User`.

## Configuration (env)

| Var | Default | |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8087` | |
| `DATABASE_URL` | required | Role owns schema `inference`. |
| `NATS_URL`, `STORAGE_URL` | required | |
| `RULES_PATH` | `config/rules.toml` | |
| `SYNC_SECS` | 15 | `inference.findings.v1` retry interval. A full page of pending writes is followed by the next at once. |
| `DB_MAX_CONNECTIONS` | 10 | Keep above `CONSUMER_CONCURRENCY` + `WRITE_CONCURRENCY`. |
| `CONSUMER_CONCURRENCY` | 4 | Documents evaluated at once per fetched batch; events of one document in order. |
| `WRITE_CONCURRENCY` | 2 | Background `inference.findings.v1` writes; beyond it, writes wait for the retry loop. |
| `A_NUMBER_CASE_LIMIT` | 50 | Cases per A-Number above which it is not joined. Part of the fingerprint: a change re-evaluates every case. |

## Notes

- The consumer delivers from the start of the stream, so on first deployment every existing case projection is replayed and indexed.
- Throughput is bounded by evaluations sharing cases or A-Numbers, which serialize on their locks, and by `CONSUMER_CONCURRENCY`. Metrics: `inference_lock_retries_total`, `inference_projections_superseded_total`, `inference_writes_deferred_total`.
- shepherd-workbench lists a case's findings, records reviews by the case's assignee, and blocks approval while a finding is open and not dismissed (`INFERENCE_URL`).

## Tests

- `cargo test -p shepherd-inference`: name matching, rule configuration, `a_number_name_mismatch`, `a_number_widely_shared`, finding diffing, event selection.
- `cargo test -p shepherd-inference -- --ignored`: end to end with Postgres, NATS and storage (Docker), including an A-Number crossing the case limit.
