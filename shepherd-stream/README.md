# shepherd-stream

Owns the NATS JetStream topology and exposes the event feed and dead-letter tooling.

- Topology: `config/topology.toml` declares streams and durable consumers. It is applied at startup with create-or-update semantics. `/healthz` only answers once the topology has been applied, and the other services wait on that in compose. Consumers bind by name (`shepherd_common::topology::bind_consumer`), so `max_deliver` and `ack_wait` are set only here.
- Streams:
  - `SHEPHERD_EVENTS` holds `shepherd.{storage,intake,search,workbench,notification,inference}.>` with unlimited retention.
  - `SHEPHERD_DLQ` holds `shepherd.dlq.<consumer>` with 30-day retention.
- Subjects are `shepherd.<service>.<entity>.<action>.<document_id>`, and `shepherd.*.*.*.<id>` selects everything for one document. Constants and payloads are in `shepherd-common/src/events.rs`.

| Event | Producer | `data` |
|---|---|---|
| `shepherd.storage.document.created` | storage (outbox) | ids, sha256, size, filename |
| `shepherd.storage.artifact.created` | storage (outbox) | ids, schema, version, sha256 |
| `shepherd.intake.extraction.requested` | intake API | reason |
| `shepherd.intake.extraction.started` | intake worker | source artifact, extractor |
| `shepherd.intake.extraction.completed` | intake worker | IR artifact id and version, page counts, `needs_ocr`, duration |
| `shepherd.intake.extraction.failed` | intake worker | error, attempt, `terminal` |
| `shepherd.intake.recognition.requested` | intake API | form type hint |
| `shepherd.intake.recognition.started` | intake recognizer | source artifact |
| `shepherd.intake.recognition.completed` | intake recognizer | form artifact, form type, field and flag counts, duration |
| `shepherd.intake.recognition.failed` | intake recognizer | error, attempt, `terminal` |
| `shepherd.search.card.indexed` | search consumer | schemas, `deleted` |
| `shepherd.workbench.receipting.required` | workbench | task id, flagged fields |
| `shepherd.workbench.receipting.completed` | workbench | receipt artifact, mode, reviewer, changed fields |
| `shepherd.workbench.case.{opened,assigned,decided,reopened}` | workbench | case id, receipt number; assignee, outcome and decision artifact, reopen reason |
| `shepherd.workbench.notice.issued` | workbench | notice id and artifact, kind, due date |
| `shepherd.notification.letter.generated` | notification | letter id, kind, letter and PDF artifacts, channels |
| `shepherd.notification.letter.held` | notification | letter id, kind, reason |
| `shepherd.notification.delivery.sent` | notification | letter and delivery ids, channel, `sent_by` |
| `shepherd.notification.delivery.failed` | notification | channel, error, attempt, `terminal` |
| `shepherd.inference.finding.raised` | inference | finding and case ids, rule, severity, related case ids |
| `shepherd.inference.finding.resolved` | inference | finding and case ids, rule |

## API

| Method | Path | Notes |
|---|---|---|
| GET | `/v1/events?document_id=` | SSE. Replays that document's history, then streams live. |
| GET | `/v1/events?subject=&from=start\|new` | SSE on any `shepherd.` subject filter (not `shepherd.dlq.`). Defaults to `shepherd.>` and live-only. |
| GET | `/v1/topology` | Streams and consumers, with message and pending counts. |
| GET | `/v1/dlq?after=&limit=` | DLQ entries with the error, the consumer, the original subject and sequence, and the payload. |
| POST | `/v1/dlq/{seq}/replay` | Admin. Republishes the entry to its original subject, then deletes it from the DLQ. |
| DELETE | `/v1/dlq/{seq}` | Admin. |
| GET | `/healthz`, `/readyz`, `/metrics`, `/openapi.json` | |

SSE frames carry `id: <stream sequence>` and `data: <envelope JSON>`. Browsers reconnect with `Last-Event-ID`, and the feed resumes from the next sequence. Each connection uses an ephemeral ordered consumer.

A replayed message reaches every consumer whose filter matches, not only the consumer that dead-lettered it. The search and intake consumers are idempotent for this reason.

## Configuration (env)

| Var | Default | |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8084` | |
| `NATS_URL` | required | |
| `TOPOLOGY_PATH` | `config/topology.toml` | |
| `ADMIN_TOKEN` | unset | Bearer token for DLQ replay and delete. Unset returns 401. |
| `CORS_ALLOWED_ORIGINS` | empty | Comma-separated list, e.g. the intake form origin. |
