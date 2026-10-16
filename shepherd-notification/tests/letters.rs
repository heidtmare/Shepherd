//! Letters end to end: events → held/generated letters → storage artifacts → mail queue and SMTP.
//! Workbench and the SMTP server are stubs; storage, Postgres and NATS are real.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{Json, Router, extract::State, routing::get};
use futures::StreamExt;
use object_store::{ObjectStore, memory::InMemory};
use reqwest::header;
use serde_json::{Value, json};
use shepherd_common::{
    events::{
        CASE_DECIDED, CASE_OPENED, CaseDecided, CaseOpened, DELIVERY_SENT, Envelope,
        LETTER_GENERATED, NOTICE_ISSUED, NoticeIssued,
    },
    storage_client::StorageClient,
};
use shepherd_storage::testing::{Infra, Running, spawn_storage, wait_ready};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

type Shared = Arc<Mutex<Value>>;

/// `GET /v1/cases/{id}` answers with the shared case detail.
async fn stub_workbench(detail: Shared) -> anyhow::Result<SocketAddr> {
    let app = Router::new()
        .route(
            "/v1/cases/{id}",
            get(|State(d): State<Shared>| async move { Json(d.lock().unwrap().clone()) }),
        )
        .route("/readyz", get(|| async { "ok" }))
        .with_state(detail);
    let l = TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?;
    tokio::spawn(async move { axum::serve(l, app).await });
    Ok(addr)
}

/// Minimal SMTP server; each accepted message's DATA is pushed to `inbox`.
async fn stub_smtp(inbox: Arc<Mutex<Vec<String>>>) -> anyhow::Result<SocketAddr> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?;
    tokio::spawn(async move {
        while let Ok((sock, _)) = l.accept().await {
            let inbox = inbox.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                let _ = w.write_all(b"220 stub ESMTP\r\n").await;
                let mut data: Option<String> = None;
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(buf) = data.as_mut() {
                        if line == "." {
                            inbox.lock().unwrap().push(data.take().unwrap());
                            let _ = w.write_all(b"250 queued\r\n").await;
                        } else {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                        continue;
                    }
                    let cmd = line.to_ascii_uppercase();
                    let reply: &[u8] = if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                        b"250 stub\r\n"
                    } else if cmd.starts_with("DATA") {
                        data = Some(String::new());
                        b"354 go\r\n"
                    } else if cmd.starts_with("QUIT") {
                        let _ = w.write_all(b"221 bye\r\n").await;
                        break;
                    } else {
                        b"250 ok\r\n"
                    };
                    let _ = w.write_all(reply).await;
                }
            });
        }
    });
    Ok(addr)
}

fn case_detail(case_id: Uuid, doc: Uuid, with_address: bool) -> Value {
    let mut facts =
        json!({ "family_name": "Nguyen", "given_name": "Linh", "a_number": "123456789" });
    if with_address {
        for (k, v) in [
            ("mailing_street", "1420 Maple Avenue"),
            ("mailing_city", "Portland"),
            ("mailing_state", "OR"),
            ("mailing_zip", "97205"),
        ] {
            facts[k] = json!(v);
        }
    }
    json!({
        "case": { "id": case_id, "receipt_number": "SHP2600000042", "form_type": "uscis.i-765",
                  "primary_document_id": doc, "status": "open", "received_date": "2026-09-01", "facts": facts },
        "benefit": { "name": "Employment Authorization" },
        "form_number": "I-765",
        "form_final": true,
        "e_notification": { "document_id": Uuid::now_v7(), "email": "linh@example.com", "mobile_phone": null },
    })
}

async fn publish<T: serde::Serialize>(
    js: &async_nats::jetstream::Context,
    kind: &str,
    doc: Uuid,
    data: T,
) -> anyhow::Result<()> {
    let ev = Envelope::new(kind, "test", doc, data);
    js.publish(ev.nats_subject(), serde_json::to_vec(&ev)?.into())
        .await?
        .await?;
    Ok(())
}

/// Stores a `workbench.decision.v1` artifact and publishes `case.decided` for it.
async fn decide(
    js: &async_nats::jetstream::Context,
    sc: &StorageClient,
    doc: Uuid,
    parent: Uuid,
    case_id: Uuid,
    decision: Value,
) -> anyhow::Result<()> {
    let art = sc
        .create_artifact(
            doc,
            "workbench.decision.v1",
            "test",
            parent,
            serde_json::to_vec(&decision)?,
        )
        .await?;
    publish(
        js,
        CASE_DECIDED,
        doc,
        CaseDecided {
            case_id,
            receipt_number: "SHP2600000042".into(),
            form_type: "uscis.i-765".into(),
            outcome: decision["outcome"].as_str().unwrap().into(),
            decided_by: "ana".into(),
            artifact_id: art.id,
            version: art.version,
        },
    )
    .await
}

/// The `notification.letter.v1` record of a generated letter.
async fn record(sc: &StorageClient, letter: &Value) -> anyhow::Result<Value> {
    let id: Uuid = letter["artifact_id"].as_str().unwrap().parse()?;
    Ok(serde_json::from_slice(&sc.content(id).await?)?)
}

async fn until<T, F, Fut>(what: &str, f: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker"]
async fn letters_end_to_end() -> anyhow::Result<()> {
    let infra = Infra::start().await?;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let storage: Running = spawn_storage(infra.storage_config(), store).await?;
    let sc = StorageClient::new(&storage.url());
    let http = reqwest::Client::new();

    let up: Value = http
        .post(format!("{}/v1/documents?filename=i765.pdf", storage.url()))
        .header(header::CONTENT_TYPE, "application/pdf")
        .body(std::fs::read(root().join("fixtures/sample.pdf"))?)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let doc: Uuid = up["document_id"].as_str().unwrap().parse()?;
    let case_id = Uuid::now_v7();

    let detail: Shared = Arc::new(Mutex::new(case_detail(case_id, doc, false)));
    let adj = stub_workbench(detail.clone()).await?;
    let inbox = Arc::new(Mutex::new(Vec::new()));
    let smtp = stub_smtp(inbox.clone()).await?;

    let cfg = shepherd_notification::Config {
        bind_addr: "127.0.0.1:0".into(),
        database_url: infra.url("notification_rw", "notification"),
        nats_url: infra.nats_url.clone(),
        storage_url: storage.url(),
        workbench_url: format!("http://{adj}"),
        letters_path: root()
            .join("shepherd-notification/config/letters.toml")
            .to_string_lossy()
            .into_owned(),
        smtp_url: Some(format!("smtp://{smtp}")),
        email_from: "Notices <notices@example.test>".into(),
        retry_secs: 3600,
        email_max_attempts: 3,
        db_max_connections: 5,
    };
    let listener = TcpListener::bind(&cfg.bind_addr).await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(shepherd_notification::run(cfg, listener, shutdown.clone()));
    wait_ready(addr, "/readyz").await?;
    let base = format!("http://{addr}");

    let nats = async_nats::connect(&infra.nats_url).await?;
    let js = async_nats::jetstream::new(nats.clone());
    let mut generated = nats.subscribe(format!("{LETTER_GENERATED}.>")).await?;
    let mut sent = nats.subscribe(format!("{DELIVERY_SENT}.>")).await?;

    let letters = |status: &'static str| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let r: Value = http
                .get(format!(
                    "{base}/v1/letters?case_id={case_id}&status={status}"
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::Ok(r["letters"].as_array().cloned().unwrap_or_default())
        }
    };

    // Case opened without a mailing address: the receipt notice is held.
    let opened = CaseOpened {
        case_id,
        receipt_number: "SHP2600000042".into(),
        form_type: "uscis.i-765".into(),
        document_id: doc,
    };
    publish(&js, CASE_OPENED, doc, &opened).await?;
    let held = until("receipt held", || async {
        Ok(letters("held").await?.into_iter().find(|l| {
            l["hold_reason"]
                .as_str()
                .is_some_and(|r| r.contains("mailing"))
        }))
    })
    .await?;
    assert_eq!(held["kind"], "receipt");
    assert!(
        held["hold_reason"]
            .as_str()
            .unwrap()
            .contains("missing street, city, state, ZIP code")
    );

    // Address corrected on the case; retry generates the letter and queues mail and e-mail.
    *detail.lock().unwrap() = case_detail(case_id, doc, true);
    let letter_id = held["id"].as_str().unwrap();
    let l: Value = http
        .post(format!("{base}/v1/letters/{letter_id}/retry"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(l["status"], "generated");
    assert_eq!(l["recipient"]["lines"][1], "Portland OR 97205");
    let ev: Envelope<Value> = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), generated.next())
            .await?
            .unwrap()
            .payload,
    )?;
    assert_eq!(ev.data["channels"], json!(["mail", "email"]));

    // PDF and JSON record are stored on the primary document.
    let d = sc.document(doc).await?;
    let pdf = d.latest("notification.letter.pdf").expect("pdf artifact");
    assert!(sc.content(pdf.id).await?.starts_with(b"%PDF-"));
    let rec: Value = serde_json::from_slice(
        &sc.content(d.latest("notification.letter.v1").unwrap().id)
            .await?,
    )?;
    assert_eq!(rec["pdf_artifact_id"], json!(pdf.id));
    assert_eq!(rec["title"], "Notice of Action: Receipt");
    let served = http
        .get(format!("{base}/v1/letters/{letter_id}/pdf"))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(served.headers()[header::CONTENT_TYPE], "application/pdf");

    // E-mail goes out to the G-1145 address without personal data beyond the receipt number.
    let mail = until("e-mail sent", || async {
        Ok(inbox.lock().unwrap().first().cloned())
    })
    .await?;
    assert!(mail.contains("Subject: Case accepted: SHP2600000042"));
    assert!(mail.contains("linh@example.com"));
    assert!(!mail.contains("Nguyen"));
    let ev: Envelope<Value> = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(10), sent.next())
            .await?
            .unwrap()
            .payload,
    )?;
    assert_eq!(ev.data["channel"], "email");

    // Redelivered event: no second letter.
    publish(&js, CASE_OPENED, doc, &opened).await?;

    // RFE: generated immediately, mail only.
    let notice_id = Uuid::now_v7();
    let notice = sc
        .create_artifact(
            doc,
            "workbench.notice.v1",
            "test",
            d.source().unwrap().id,
            serde_json::to_vec(&json!({
                "schema": "workbench.notice.v1", "notice_id": notice_id, "kind": "rfe",
                "items": ["Copy of passport"], "message": null, "due_date": "2026-12-20",
                "issued_at": "2026-09-24T15:00:00Z",
            }))?,
        )
        .await?;
    publish(
        &js,
        NOTICE_ISSUED,
        doc,
        NoticeIssued {
            case_id,
            receipt_number: "SHP2600000042".into(),
            notice_id,
            kind: "rfe".into(),
            artifact_id: notice.id,
            due_date: "2026-12-20".into(),
        },
    )
    .await?;
    let rfe = until("rfe generated", || async {
        Ok(letters("generated")
            .await?
            .into_iter()
            .find(|l| l["kind"] == "rfe"))
    })
    .await?;
    let deliveries = rfe["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0]["channel"], "mail");

    // Mail room records the mailing.
    let queue: Value = http
        .get(format!("{base}/v1/deliveries?channel=mail&status=queued"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(queue["deliveries"].as_array().unwrap().len(), 2);
    let did = deliveries[0]["id"].as_str().unwrap();
    let mailed = http
        .post(format!("{base}/v1/deliveries/{did}/mailed"))
        .header("x-user", "mailroom")
        .json(&json!({ "reference": "BATCH-7" }))
        .send()
        .await?;
    assert_eq!(mailed.status(), 200);
    let again = http
        .post(format!("{base}/v1/deliveries/{did}/mailed"))
        .header("x-user", "mailroom")
        .send()
        .await?;
    assert_eq!(again.status(), 409);

    assert_eq!(letters("held").await?.len(), 0);
    assert_eq!(letters("generated").await?.len(), 2);
    assert_eq!(inbox.lock().unwrap().len(), 1);

    let generated_kind = |kind: &'static str| {
        let letters = &letters;
        async move {
            anyhow::Ok(
                letters("generated")
                    .await?
                    .into_iter()
                    .find(|l| l["kind"] == kind),
            )
        }
    };

    // NOID: reasons listed as items, officer message, response due date.
    let noid_id = Uuid::now_v7();
    let noid_art = sc
        .create_artifact(
            doc,
            "workbench.notice.v1",
            "test",
            d.source().unwrap().id,
            serde_json::to_vec(&json!({
                "schema": "workbench.notice.v1", "notice_id": noid_id, "kind": "noid",
                "items": ["The record does not show a pending asylum application."],
                "message": "Submit the receipt notice for Form I-589.", "due_date": "2026-10-27",
                "issued_at": "2026-09-24T15:00:00Z",
            }))?,
        )
        .await?;
    publish(
        &js,
        NOTICE_ISSUED,
        doc,
        NoticeIssued {
            case_id,
            receipt_number: "SHP2600000042".into(),
            notice_id: noid_id,
            kind: "noid".into(),
            artifact_id: noid_art.id,
            due_date: "2026-10-27".into(),
        },
    )
    .await?;
    let noid = until("noid generated", || generated_kind("noid")).await?;
    assert_eq!(noid["source_id"], json!(noid_id));
    let rec = record(&sc, &noid).await?;
    assert_eq!(rec["title"], "Notice of Intent to Deny");
    assert_eq!(rec["letter_date"], "2026-09-24");
    let blocks = rec["blocks"].as_array().unwrap();
    assert!(blocks.contains(
        &json!({ "type": "heading", "text": "Reasons and evidence that may overcome them" })
    ));
    assert!(blocks.contains(&json!({ "type": "numbered",
        "text": ["The record does not show a pending asylum application."] })));
    assert!(blocks.contains(
        &json!({ "type": "paragraph", "text": "Submit the receipt notice for Form I-589." })
    ));
    assert!(
        rec["details"]
            .as_array()
            .unwrap()
            .contains(&json!(["Response due", "October 27, 2026"]))
    );

    // Denial for abandonment: grounds and rationale, dated the decision date.
    decide(
        &js,
        &sc,
        doc,
        noid_art.id,
        case_id,
        json!({ "schema": "workbench.decision.v1", "outcome": "denied",
                "grounds": [{ "id": "abandonment",
                    "label": "Abandonment: no response to a request for evidence or notice of intent to deny" }],
                "rationale": "No response to the notice of intent to deny was received by the due date.",
                "decided_at": "2026-10-30T14:00:00Z" }),
    )
    .await?;
    let denial = until("denial generated", || generated_kind("denial")).await?;
    let rec = record(&sc, &denial).await?;
    assert_eq!(rec["title"], "Decision: Denial");
    assert_eq!(rec["letter_date"], "2026-10-30");
    let blocks = rec["blocks"].as_array().unwrap();
    assert!(blocks.contains(&json!({ "type": "heading", "text": "Grounds for denial" })));
    assert!(blocks.contains(&json!({ "type": "bullets",
        "text": ["Abandonment: no response to a request for evidence or notice of intent to deny"] })));
    assert!(blocks.contains(&json!({ "type": "paragraph",
        "text": "No response to the notice of intent to deny was received by the due date." })));

    // Administrative closure: no letter. Withdrawal: acknowledgment without grounds. Events are
    // consumed in order, so the withdrawal letter implies the closure was handled.
    decide(
        &js,
        &sc,
        doc,
        noid_art.id,
        case_id,
        json!({ "schema": "workbench.decision.v1", "outcome": "closed",
                "closure_reason": "duplicate", "rationale": "Duplicate of an earlier filing.",
                "decided_at": "2026-11-02T14:00:00Z" }),
    )
    .await?;
    decide(
        &js,
        &sc,
        doc,
        noid_art.id,
        case_id,
        json!({ "schema": "workbench.decision.v1", "outcome": "withdrawn", "grounds": [],
                "rationale": "Applicant withdrew the request by signed letter.",
                "decided_at": "2026-11-03T14:00:00Z" }),
    )
    .await?;
    let withdrawal = until("withdrawal generated", || generated_kind("withdrawal")).await?;
    let rec = record(&sc, &withdrawal).await?;
    assert_eq!(rec["title"], "Notice of Action: Withdrawal Acknowledged");
    assert_eq!(rec["letter_date"], "2026-11-03");
    assert!(
        rec["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["type"] == "paragraph")
    );

    // One letter per event, mail only; e-mail is configured for the receipt notice alone.
    let all = letters("generated").await?;
    let mut kinds: Vec<&str> = all.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    kinds.sort();
    assert_eq!(kinds, ["denial", "noid", "receipt", "rfe", "withdrawal"]);
    assert_eq!(letters("held").await?.len(), 0);
    let queue: Value = http
        .get(format!("{base}/v1/deliveries?channel=mail&status=queued"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(queue["deliveries"].as_array().unwrap().len(), 4);
    assert_eq!(inbox.lock().unwrap().len(), 1);

    shutdown.cancel();
    handle.await??;
    storage.stop().await?;
    Ok(())
}
