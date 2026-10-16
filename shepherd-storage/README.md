# shepherd-storage

System of record for source PDFs and derived artifacts (JSON, or PDF such as generated letters).

- Blobs: S3 API (`object_store`), content-addressed at `sha256/<ab>/<cd>/<hash>`. Identical bytes are stored once.
- Metadata: Postgres schema `storage`. `artifacts` and `audit_log` are append-only (enforced by trigger).
- Events: written to `storage.outbox` in the same transaction as the data, then relayed to the JetStream stream `SHEPHERD_EVENTS` (declared by shepherd-stream), with `Nats-Msg-Id` set for dedupe.

## API

| Method | Path | Notes |
|---|---|---|
| POST | `/v1/documents?filename=` | Body is the raw PDF (`Content-Type: application/pdf`), streamed. Checks the `%PDF-` signature. `X-Actor` is recorded in the audit log. |
| GET | `/v1/documents?cursor=&limit=` | Keyset pagination by id. |
| GET | `/v1/documents/{id}` | Metadata plus all artifact versions. |
| GET | `/v1/documents/{id}/source` | Streams the PDF. Supports a single `Range`, and `If-None-Match` (ETag = sha256). |
| GET | `/v1/documents/{id}/audit` | |
| POST | `/v1/documents/{id}/artifacts?schema=&produced_by=&parent_artifact_id=` | Body is JSON (`Content-Type: application/json` or absent), or a PDF (`application/pdf`, checks the `%PDF-` signature). Version = previous max for `(document, schema)` + 1. Size limit `MAX_JSON_BYTES` for both. |
| GET | `/v1/artifacts/{id}`, `/v1/artifacts/{id}/content` | |
| GET | `/healthz`, `/readyz`, `/metrics`, `/openapi.json` | |

Events: `shepherd.storage.document.created.<doc_id>` and `shepherd.storage.artifact.created.<doc_id>`. The envelope is defined in `shepherd-common/src/events.rs`. `data` carries ids and hashes only, never the content.

## Configuration (env)

| Var | Default | |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8081` | |
| `DATABASE_URL` | required | Role needs ownership of schema `storage`. |
| `NATS_URL` | unset | Unset disables the relay; events queue in the outbox. |
| `BLOB_BACKEND` | `s3` | `s3`, `local`, or `memory`. |
| `BLOB_BUCKET`, `BLOB_ENDPOINT`, `BLOB_REGION`, `BLOB_ACCESS_KEY_ID`, `BLOB_SECRET_ACCESS_KEY`, `BLOB_ALLOW_HTTP` | | S3 settings. When `BLOB_ENDPOINT` is set, path-style addressing is used. |
| `BLOB_LOCAL_PATH` | | For the `local` backend. |
| `MAX_PDF_BYTES` | 512 MiB | |
| `MAX_JSON_BYTES` | 64 MiB | |
| `OUTBOX_BATCH`, `OUTBOX_POLL_MS`, `OUTBOX_RETENTION_DAYS` | 256, 500, 7 | Published outbox rows are pruned hourly after the retention period, in batches of 5000 via `outbox_published_idx`. |
| `DB_MAX_CONNECTIONS` | 20 | |
| `BLOB_GC_INTERVAL_HOURS`, `BLOB_GC_GRACE_HOURS` | 24, 24 | See Blob GC. Interval 0 disables GC. |
| `RUST_LOG`, `LOG_FORMAT` | `info`, json | `LOG_FORMAT=pretty` gives human-readable output. |

## Notes

- Artifact uploads check the document and `parent_artifact_id` before writing the blob, so rejected requests leave no blob behind.
- Uploads over 8 MiB use S3 multipart to a `tmp/` key, followed by a rename to the content key. Incomplete multipart uploads are not objects; expire them with a bucket lifecycle rule.

## Blob GC

A failed transaction after a blob upload leaves an unreferenced blob, and an interrupted rename leaves a `tmp/` object. Every `BLOB_GC_INTERVAL_HOURS`, one replica (advisory lock) sweeps them:

- It lists `sha256/` and `tmp/`, skipping objects modified within `BLOB_GC_GRACE_HOURS`.
- It deletes `tmp/` objects, and `sha256/` objects with no `storage.artifacts.object_key` (`artifacts_object_key_idx`).
- Each content delete re-checks references under an exclusive advisory lock on the key.
- An upload that finds its content already stored takes that lock shared and confirms the object still exists before inserting its row. If GC deleted it in the meantime, an artifact upload re-writes the object. A document upload, whose streamed body cannot be replayed, returns 503 and the client retries.
- Listing cost is linear in the bucket size: about one request per 1000 objects.
