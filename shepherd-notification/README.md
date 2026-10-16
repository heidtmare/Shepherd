# shepherd-notification

Outbound correspondence for benefit cases. Letters are rendered to PDF, stored in shepherd-storage, queued for print and mail, and e-mailed where the applicant filed an e-notification request (G-1145).

- Input: durable consumer `notification-correspondence` on `shepherd.workbench.case.opened.*`, `shepherd.workbench.notice.issued.*` and `shepherd.workbench.case.decided.*`. Case data is read from shepherd-workbench (`GET /v1/cases/{id}`), notice and decision content from storage.
- Output: `notification.letter.pdf` (the letter, `application/pdf`) and `notification.letter.v1` (JSON record) on the case's primary document. Events: `shepherd.notification.letter.{generated,held}` and `shepherd.notification.delivery.{sent,failed}`, with the primary document as the subject. shepherd-workbench records them in the case history.

## Letters

| Event | Kind | Content |
|---|---|---|
| `case.opened` | `receipt` | Receipt notice. Held until the primary form's values are final (receipted, or recognized with nothing flagged). |
| `notice.issued` (`rfe`, `noid`) | `rfe`, `noid` | Requested items, officer message, due date. Dated the issue date. |
| `case.decided` | `approval`, `denial`, `withdrawal` | Denial lists the grounds and the rationale. Dated the decision date. A decision after a reopening produces a new letter. Administrative closure (`closed`) produces none. |

One letter per triggering entity (`(kind, source_id)`: the case, the notice, or the decision artifact), so redelivery and DLQ replay are no-ops.

- Addressee: applicant (petitioner for I-130) name and the `mailing_*` case facts at generation time. US addresses need street, city, state and ZIP; others need street, city and country.
- Held letters: `status = held` with `hold_reason` (incomplete address, receipting pending, case not found). Retried every `RETRY_SECS` and on `POST /v1/letters/{id}/retry`. Fix the data in the workbench (e.g. edit the mailing address facts) and retry. A failed attempt (workbench or storage error, unreadable source artifact) sets `last_error` and moves the letter to the back of the retry scan.
- Text: `config/letters.toml`, with letterhead, signature, and title, body and closing per kind. Placeholders are validated at startup.
- PDF: US Letter, Helvetica (base-14, not embedded), WinAnsi encoding (characters outside Latin-1 print as `?`). Address block positioned for a #10 window envelope. Output is deterministic for a given letter; `notification.letter.v1` carries `pdf_sha256`.

`notification.letter.v1`:

```json
{
  "schema": "notification.letter.v1", "letter_id": "…", "case_id": "…",
  "source_id": "…", "source_artifact_id": "…", "pdf_artifact_id": "…", "pdf_sha256": "…",
  "channels": ["mail", "email"], "generated_at": "…",
  "kind": "rfe", "title": "Request for Evidence", "receipt_number": "SHP2600000042", "letter_date": "2026-09-24",
  "letterhead": ["…"], "details": [["Receipt number", "SHP2600000042"], ["Response due", "December 20, 2026"]],
  "recipient": { "name": "Linh Nguyen", "lines": ["1420 Maple Avenue # 4B", "Portland OR 97205"] },
  "blocks": [{ "type": "paragraph", "text": "…" }, { "type": "heading", "text": "Evidence requested" }, { "type": "numbered", "text": ["…"] }],
  "signature": ["Director", "…"]
}
```

## Deliveries

| Channel | When | Lifecycle |
|---|---|---|
| `mail` | Every letter. | `queued` until the mail room records the mailing (`POST /v1/deliveries/{id}/mailed`), then `sent` with `sent_by` and `reference`. |
| `email` | Receipt notices, when the case has a G-1145 with an e-mail address. | Sent over SMTP. Failures retry with backoff (30 s doubling, capped at 1 h); after `EMAIL_MAX_ATTEMPTS` the delivery is `failed` and can be requeued. Without `SMTP_URL`, stays `queued`. |

The e-mail carries the form number, receipt number and received date only; the letter is not attached. Text messages to the G-1145 mobile number are not sent.

## API

| Method | Path | |
|---|---|---|
| GET | `/v1/letters?case_id=&status=held\|generated&kind=&limit=` | Newest first, with deliveries. |
| GET | `/v1/letters/{id}` | |
| GET | `/v1/letters/{id}/pdf` | |
| POST | `/v1/letters/{id}/retry` | Held letters only; 409 otherwise. |
| GET | `/v1/deliveries?channel=mail\|email&status=queued\|sent\|failed&limit=` | Oldest first. `channel=mail&status=queued` is the print queue. |
| POST | `/v1/deliveries/{id}/mailed` `{reference?}` | `X-User` required. Queued mail only; 409 otherwise. |
| POST | `/v1/deliveries/{id}/retry` | `X-User` required. Failed deliveries only. |
| GET | `/healthz`, `/readyz`, `/metrics`, `/openapi.json` | |

No authentication; deploy behind an auth proxy that sets `X-User`. The workbench reaches letters through shepherd-workbench (`NOTIFICATION_URL`).

## Configuration (env)

| Var | Default | |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8086` | |
| `DATABASE_URL` | required | Role owns schema `notification`. |
| `NATS_URL`, `STORAGE_URL`, `WORKBENCH_URL` | required | |
| `LETTERS_PATH` | `config/letters.toml` | |
| `SMTP_URL` | unset | `smtp://host:port`, `smtp://host?tls=required` (STARTTLS), `smtps://user:pass@host`. Unset leaves e-mail queued. |
| `EMAIL_FROM` | `Shepherd Notices <notices@shepherd.invalid>` | |
| `RETRY_SECS` | 60 | Held-letter retry and e-mail send interval. |
| `EMAIL_MAX_ATTEMPTS` | 5 | |
| `DB_MAX_CONNECTIONS` | 5 | |

## Notes

- The consumer is declared with `deliver_from = "new"`: when it is first created, earlier case events are not replayed, so existing cases get no letters.
- E-mail is decided when the receipt notice is generated. A G-1145 attached to the case afterwards does not trigger one.

- Generation holds the letter's row lock while it calls workbench and storage. A failure after the artifacts are stored and before commit leaves orphan artifact versions; the retry stores new ones.
- Letters use the case facts current at generation, not the snapshot in the notice or decision artifact, so a corrected address releases a held letter.

## Tests

- `cargo test -p shepherd-notification`: composition, addresses, holds, PDF layout and pagination, event mapping.
- `cargo test -p shepherd-notification -- --ignored`: end to end with Postgres, NATS and storage (Docker); workbench and SMTP are stubs.
