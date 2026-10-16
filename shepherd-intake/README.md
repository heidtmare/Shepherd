# shepherd-intake

Accepts PDFs, extracts the text layer and stores an `intake.ir.v1` artifact for each source document. It serves the upload form at `/`.

- Upload: `POST /v1/intake` streams the body to storage (`POST /v1/documents`). Extraction starts from the resulting `document.created` event, so documents uploaded directly to storage are processed the same way.
- Worker: bound to the durable `intake-extractor`, which filters on `shepherd.storage.document.created.*` and `shepherd.intake.extraction.requested.*`. For each event the worker:
  1. Fetches the source through storage and verifies its sha256.
  2. Extracts text in a blocking task with a timeout.
  3. Stores the IR with `parent_artifact_id` set to the source and `produced_by = shepherd-intake/<version>`.
  4. Emits `extraction.started`, `extraction.completed` and `extraction.failed` status events.
- Idempotency: an automatic trigger is skipped if an IR from the same extractor version already exists for the current source artifact. `POST /v1/intake/{id}/reprocess` always produces a new version.
- Failure classes:
  - Unreadable or encrypted PDFs, a missing document, a hash mismatch, or a 4xx from storage are terminal: a `failed` event with `terminal: true` is emitted and the message goes to `shepherd.dlq.intake-extractor`.
  - Anything else is retried with backoff up to the consumer's `max_deliver`.
- Extraction: `pdf-extract` (pure Rust), with no OCR. Pages with fewer than 16 non-whitespace characters are counted as having no text layer. Such pages appear in `flags.empty_pages`, and `flags.needs_ocr` is set. Per-page errors, including extractor panics, are recorded on the page and do not fail the document.

## Form recognition (`intake.form.v1`)

A second durable consumer, `intake-recognizer`, reads handwritten form fields with `shepherd-forms`. It filters on `shepherd.storage.document.created.*` and `shepherd.intake.recognition.requested.*`.

- Form type: taken from the request, else the upload label `form_type` (`POST /v1/intake?form_type=uscis.i-765`), else automatic classification. Either may be a form type (the edition is then chosen among its editions) or a template id (that edition exactly).
- Output: an `intake.form.v1` artifact containing:
  - `form`: the template match, method, score and per-page alignment shifts
  - `stats`
  - one result per template field: value, raw reading, confidence, lowest token probability, status (`filled`, `empty` or `requires_receipting`), reasons and ink box
- Events: `shepherd.intake.recognition.{started,completed,failed}`. The completed event carries `form_type`, `template_id` (edition) and `requires_receipting`; workbench opens a task when `requires_receipting` is non-zero.
- Idempotency: automatic triggers are skipped when a form artifact from this recognizer version exists for the source. `POST /v1/intake/{id}/recognize {form_type?}` always produces a new version.
- Load: recognition takes about 1 s per page plus about 0.6 s per handwritten field on CPU. Keep `RECOGNITION_CONCURRENCY` at 1 and scale by replicas; each process loads the 1.3 GB model.
- Timeouts: a recognition that exceeds `RECOGNITION_TIMEOUT_SECS` is retried, but its blocking task cannot be cancelled. It keeps its concurrency slot until it returns, so the retry waits instead of running alongside it. While a message waits for a slot it sends progress acks every 30 s, so `ack_wait` does not expire. Counter: `recognition_timeouts_total`.
- Blank cache: rendered blanks are kept up to `RECOGNITION_BLANK_CACHE_MB`, least recently used evicted first. A 7-page form takes about 26 MB at 200 dpi.

## `intake.ir.v1`

```json
{
  "schema": "intake.ir.v1",
  "extractor": { "name": "shepherd-intake", "version": "0.1.0", "engine": "pdf-extract/0.12" },
  "source": { "document_id": "…", "artifact_id": "…", "sha256": "…", "filename": "claim.pdf", "size_bytes": 1234 },
  "metadata": { "title": "…", "author": "…", "subject": "…", "keywords": null, "creator": null, "producer": "…",
                "created": "2026-08-28T14:30:00Z", "modified": null, "pdf_version": "1.7", "encrypted": false },
  "title": "…",
  "pages": [{ "number": 1, "text": "…", "chars": 53, "has_text": true, "error": null }],
  "stats": { "pages": 3, "text_pages": 2, "empty_pages": 1, "chars": 129 },
  "flags": { "needs_ocr": true, "empty_pages": [3], "failed_pages": [] }
}
```

`title` is the PDF metadata title, falling back to the first non-empty line of text. Dates are RFC 3339. Types are defined in `src/ir.rs`, and `fixtures/sample.ir.json` is checked against them in tests.

## API

| Method | Path | Notes |
|---|---|---|
| GET | `/` | Upload form. It follows each document through the stream service's SSE feed. |
| POST | `/v1/intake?filename=&form_type=&receipt_number=&received_date=` | Raw PDF body. Returns 202 `{document_id, artifact_id, sha256, size_bytes, events_url}`. Storage errors are passed through. `receipt_number` (`AAA` + 10 digits) and `received_date` (`YYYY-MM-DD`) are stored as labels; workbench uses them to link the document to a case. |
| POST | `/v1/intake/{id}/reprocess` | Queues re-extraction. Returns 202. |
| POST | `/v1/intake/{id}/recognize` | Queues form recognition, with an optional `{form_type}`. Returns 202. |
| GET | `/v1/form-types` | Recognizable templates. |
| GET | `/ui/config.json` | Browser-facing service URLs. |
| GET | `/healthz`, `/readyz`, `/metrics`, `/openapi.json` | |

There is no authentication on upload or reprocess. Put intake behind the deployment's auth proxy.

## Configuration (env)

| Var | Default | |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8083` | |
| `NATS_URL` | required | |
| `STORAGE_URL` | required | Service-to-service URL. |
| `PUBLIC_STORAGE_URL`, `PUBLIC_SEARCH_URL`, `PUBLIC_STREAM_URL` | `http://localhost:808{1,2,4}` | Used by the browser form. The stream origin must allow this service's origin (`CORS_ALLOWED_ORIGINS`). |
| `CONCURRENCY` | 4 | Parallel extractions. |
| `EXTRACT_TIMEOUT_SECS` | 120 | A timeout is treated as transient. The blocking task still runs to completion. |
| `MAX_PAGE_CHARS` | 200000 | Per-page text cap. |
| `RECOGNITION_ENABLED` | true | Startup fails if enabled and pdfium or the templates are missing. A missing model only disables HTR, which flags every handwritten field. |
| `FORMS_DIR`, `PDFIUM_LIB_DIR`, `HTR_MODEL_DIR` | `forms`, `vendor/pdfium/lib`, `vendor/models/trocr-base-handwritten` | |
| `RECOGNITION_CONCURRENCY`, `RECOGNITION_TIMEOUT_SECS` | 1, 540 | Slots are held until a timed-out task finishes. |
| `RECOGNITION_BLANK_CACHE_MB` | 256 | Rendered template blanks. |
| `PUBLIC_WORKBENCH_URL` | `http://localhost:8085` | Link target on the upload page. |
