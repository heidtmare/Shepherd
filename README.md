# shepherd

End-to-end document processing and adjudication. PDFs are stored, extracted, recognized, indexed and routed to human review; USCIS benefit forms are used as a usecase example. Cases are receipted, reviewed and decided, and notices are sent to applicants.

Rust workspace of services with one responsibility each. Services communicate through NATS JetStream events and storage's HTTP API; each owns a Postgres schema in a shared database.

## Architecture

```mermaid
flowchart LR
    user([Uploader]) -->|PDF| intake
    officer([Reviewer / officer]) --> workbench

    subgraph services
        intake[shepherd-intake<br/>:8083]
        storage[shepherd-storage<br/>:8081]
        search[shepherd-search<br/>:8082]
        stream[shepherd-stream<br/>:8084]
        workbench[shepherd-workbench<br/>:8085]
        notification[shepherd-notification<br/>:8086]
        inference[shepherd-inference<br/>:8087]
    end

    subgraph infra
        pg[(Postgres<br/>storage / search / workbench / notification / inference schemas)]
        s3[(S3 object store)]
        nats[[NATS JetStream<br/>SHEPHERD_EVENTS / SHEPHERD_DLQ]]
        smtp[(SMTP)]
    end

    intake -->|upload, store artifacts| storage
    workbench -->|fetch, store artifacts| storage
    workbench -->|discovery| search
    workbench -->|re-recognize| intake
    workbench -->|letters| notification
    workbench -->|findings| inference
    notification -->|case data| workbench
    notification -->|store letters| storage
    notification --> pg
    notification -->|e-notification| smtp
    inference -->|case data, store findings| storage
    inference --> pg

    storage --> pg
    storage --> s3
    search --> pg
    search -->|artifact JSON| s3
    workbench --> pg

    storage -. outbox relay .-> nats
    intake <-. events .-> nats
    search <-. events .-> nats
    workbench <-. events .-> nats
    notification <-. events .-> nats
    inference <-. events .-> nats
    stream -->|topology, SSE, DLQ| nats
```

Solid lines are HTTP or database access; dotted lines are events.

## Services

| Crate | Port | Responsibility |
|---|---|---|
| [shepherd-storage](shepherd-storage/README.md) | 8081 | System of record. Source PDFs and derived JSON artifacts, content-addressed in S3, metadata in Postgres. Append-only artifacts and audit log. Publishes `storage.*` events through a transactional outbox. |
| [shepherd-search](shepherd-search/README.md) | 8082 | Card catalog. Projects each document's current state into Postgres full-text search (`search.cards`, `search.pages`). Query API with filters, facets, keyset pagination. Rebuildable from storage. |
| [shepherd-intake](shepherd-intake/README.md) | 8083 | Upload endpoint and form. Extracts the PDF text layer to `intake.ir.v1`, and recognizes handwritten form fields to `intake.form.v1` via shepherd-forms. |
| [shepherd-stream](shepherd-stream/README.md) | 8084 | Owns the JetStream topology (streams, durable consumers, retry limits). SSE event feed per document or subject. DLQ inspection and replay. |
| [shepherd-workbench](shepherd-workbench/README.md) | 8085 | Human-in-the-loop workbench. Receipting (verify or enter form values), case management, workloads (skill- and capacity-based rule assignment, worker requests), eligibility assessment, RFE/NOID notices, approve/deny/withdraw decisions. |
| [shepherd-notification](shepherd-notification/README.md) | 8086 | Outbound correspondence. Receipt, RFE, NOID and decision letters rendered to PDF and stored in storage; print-and-mail queue; e-mail for G-1145 e-notification requests. |
| [shepherd-inference](shepherd-inference/README.md) | 8087 | Rules on case data. Indexes the parties of every case from its projection, evaluates configured rules (e.g. A-Number recorded under different names) across cases, and records findings for review. |

Libraries:

| Crate | Purpose |
|---|---|
| shepherd-common | Event envelope and subjects, consumer binding, blob access, storage client, config, telemetry, health checks. |
| [shepherd-forms](shepherd-forms/README.md) | Form templates from fillable PDFs, classification, page alignment, ink isolation, TrOCR handwriting recognition, value validation. CLI for templates and synthetic scans. |

## Document flow

```mermaid
sequenceDiagram
    autonumber
    participant U as Uploader
    participant I as intake
    participant S as storage
    participant N as JetStream
    participant X as search
    participant A as workbench
    participant L as notification
    participant R as Reviewer

    U->>I: POST /v1/intake (PDF)
    I->>S: POST /v1/documents
    S-->>N: storage.document.created
    par extraction
        N->>I: intake-extractor
        I->>S: store intake.ir.v1
    and recognition
        N->>I: intake-recognizer
        I->>S: store intake.form.v1
        I-->>N: intake.recognition.completed
    end
    S-->>N: storage.artifact.created
    N->>X: search-indexer rebuilds card
    N->>A: workbench-receipting
    A->>A: open receipting task and/or case
    R->>A: receipt flagged fields
    A->>S: store receipting.form.v1
    R->>A: assess, notice, decide
    A->>S: store workbench.case.v1 / notice / decision
    S-->>N: storage.artifact.created
    N->>X: card updated (case discoverable)
    A-->>N: case.opened / notice.issued / case.decided
    N->>L: notification-correspondence
    L->>S: store notification.letter.pdf / .v1
    L-->>N: letter.generated, delivery.sent
    N->>A: workbench-correspondence (case history)
```

Every artifact write goes through storage, so every derived result produces `storage.artifact.created` and is re-indexed by search.

## Events

Subjects: `shepherd.<service>.<entity>.<action>.<document_id>`. Envelope in `shepherd-common/src/events.rs`; payloads carry ids and hashes, not content. Full list in [shepherd-stream/README.md](shepherd-stream/README.md).

```mermaid
flowchart LR
    subgraph SHEPHERD_EVENTS
        st["shepherd.storage.>"]
        in["shepherd.intake.>"]
        se["shepherd.search.>"]
        ad["shepherd.workbench.>"]
        no["shepherd.notification.>"]
        nf["shepherd.inference.>"]
    end

    st -->|document.created, artifact.created| c1[search-indexer]
    st -->|document.created| c2[intake-extractor]
    in -->|extraction.requested| c2
    st -->|document.created| c3[intake-recognizer]
    in -->|recognition.requested| c3
    in -->|recognition.completed| c4[workbench-receipting]
    ad -->|case.opened, notice.issued, case.decided| c5[notification-correspondence]
    no -->|letter.*, delivery.*| c6[workbench-correspondence]
    st -->|artifact.created: workbench.case.v1| c7[inference-rules]

    c1 & c2 & c3 & c4 & c5 & c6 & c7 -. max_deliver exceeded / terminal .-> dlq["SHEPHERD_DLQ<br/>shepherd.dlq.#lt;consumer#gt;"]
```

Consumers are declared only in `shepherd-stream/config/topology.toml`; services bind by name. Consumers are idempotent, so redelivery and DLQ replay are safe.

## Artifacts

All derived data is a versioned JSON artifact attached to a document. Versions increment per `(document, schema)`.

```mermaid
flowchart TD
    pdf[source PDF] --> ir[intake.ir.v1<br/>text layer]
    pdf --> form[intake.form.v1<br/>recognized field values]
    form --> rcpt[receipting.form.v1<br/>human-verified values]
    rcpt --> case[workbench.case.v1<br/>case projection]
    form --> case
    case --> notice[workbench.notice.v1<br/>RFE / NOID]
    case --> dec[workbench.decision.v1<br/>approve / deny / withdraw]
    case --> letter[notification.letter.pdf + .v1<br/>receipt notice]
    notice --> letter
    dec --> letter
    case --> find[inference.findings.v1<br/>rule findings]
```

## Case lifecycle

```mermaid
stateDiagram-v2
    [*] --> open: benefit form recognized or receipted
    open --> in_review: assign (manager or rule)
    in_review --> open: unassign
    in_review --> awaiting_response: RFE / NOID issued
    awaiting_response --> in_review: response received
    in_review --> decided: approve / deny / withdraw
    awaiting_response --> decided: withdraw, or deny (abandonment) after due date
    decided --> in_review: reopen
```

Configured benefits: I-765, I-130, I-90 (`shepherd-workbench/config/benefits.toml`). Recognizable forms: G-1145, I-765, I-90, I-130 (`forms/templates/`).

## Repository layout

| Path | Contents |
|---|---|
| `shepherd-*/` | Workspace crates. |
| `forms/` | Blank USCIS PDFs, generated templates, handwriting fonts, synthetic samples. |
| `fixtures/` | Test PDFs and sample artifacts. |
| `docker/` | Service Dockerfile (`SERVICE` build arg), Postgres init (roles, schemas). |
| `vendor/` | pdfium and TrOCR model, populated by `scripts/fetch-deps.sh`. |
| `scripts/` | `fetch-deps.sh`, `smoke.sh`, `smoke-receipting.sh`, `seed.sh`, `bench.sh`. |

## Running

```sh
cp .env.example .env        # optional overrides
docker compose up --build
```

First start downloads the TrOCR model (~1.3 GB) into the `models` volume.

| URL | |
|---|---|
| http://localhost:8083 | Upload form |
| http://localhost:8085 | Workbench |
| http://localhost:8086 | Notification API (letters, mail queue) |
| http://localhost:8087 | Inference API (rules, findings) |
| http://localhost:8025 | Captured e-mail (mailpit) |
| http://localhost:9001 | Object store console |

Each service exposes `/healthz`, `/readyz`, `/metrics` and `/openapi.json`. No service authenticates requests; deploy behind an auth proxy (workbench takes the user from `X-User` and roles from `X-Roles`).

## Testing

```sh
scripts/fetch-deps.sh                 # pdfium + TrOCR into vendor/
cargo test --workspace                # unit tests
cargo test --workspace -- --ignored   # integration tests (Docker via testcontainers)
scripts/smoke.sh                      # upload → extract → search, against a running stack
scripts/smoke-receipting.sh           # scan → recognition → receipt → search
scripts/seed.sh 1000000 && scripts/bench.sh   # search load test (requires oha)
```

Recognition end-to-end tests need `--release`; see each crate's README.

## Third-party assets

| Asset | License |
|---|---|
| `forms/uscis/*.pdf` | U.S. government work, public domain. |
| `forms/fonts/Caveat.ttf`, `NothingYouCouldDo.ttf` | SIL OFL 1.1 (`forms/fonts/OFL-*.txt`). |
| `forms/fonts/HomemadeApple.ttf` | Apache 2.0 (`forms/fonts/LICENSE-HomemadeApple.txt`). |
| pdfium (downloaded, not committed) | BSD-3-Clause and bundled licenses. |
| TrOCR `trocr-base-handwritten` (downloaded, not committed) | MIT. |

Sample form values, names and e-mail addresses are synthetic. Not affiliated with or endorsed by USCIS or DHS.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
