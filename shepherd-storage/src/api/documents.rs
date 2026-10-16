use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use shepherd_common::events::{DOCUMENT_CREATED, DocumentCreated, Envelope};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{AppState, actor, serve_blob};
use crate::{
    SERVICE,
    db::{self, Artifact, AuditEntry, Document, NewArtifact, NewDocument, SOURCE_SCHEMA},
    error::{ApiError, ApiResult},
    gc,
};

const PDF: &str = "application/pdf";

#[derive(Deserialize, IntoParams)]
pub struct CreateDocumentParams {
    /// Original filename, stored for display and search.
    filename: Option<String>,
    /// JSON object of string labels, e.g. `{"form_type":"uscis.i-765"}`.
    labels: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedDocument {
    document_id: Uuid,
    artifact_id: Uuid,
    sha256: String,
    size_bytes: i64,
}

/// Upload a PDF. The body is the raw file, streamed to the blob store.
#[utoipa::path(
    post, path = "/v1/documents", params(CreateDocumentParams),
    request_body(content = Vec<u8>, content_type = "application/pdf"),
    responses((status = 201, body = CreatedDocument), (status = 413), (status = 415))
)]
pub async fn create_document(
    State(st): State<AppState>,
    Query(p): Query<CreateDocumentParams>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<(StatusCode, Json<CreatedDocument>)> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ct.trim().to_ascii_lowercase().starts_with(PDF) {
        return Err(ApiError::UnsupportedMediaType(format!(
            "content-type must be {PDF}"
        )));
    }
    let actor = actor(&headers);
    let labels: serde_json::Map<String, serde_json::Value> = match &p.labels {
        None => Default::default(),
        Some(raw) => {
            let v: serde_json::Map<String, serde_json::Value> = serde_json::from_str(raw)
                .map_err(|_| ApiError::BadRequest("labels must be a JSON object".into()))?;
            if v.len() > 32
                || v.iter()
                    .any(|(k, v)| k.len() > 64 || v.as_str().is_none_or(|s| s.len() > 256))
            {
                return Err(ApiError::BadRequest(
                    "labels: at most 32 string values, keys <= 64 and values <= 256 chars".into(),
                ));
            }
            v
        }
    };
    let stored = st
        .blobs
        .put_stream(
            body.into_data_stream(),
            st.cfg.max_pdf_bytes,
            Some(b"%PDF-"),
        )
        .await?;

    let doc_id = Uuid::now_v7();
    let art_id = Uuid::now_v7();
    let size = stored.size as i64;
    let mut tx = st.db.begin().await?;
    if stored.existed && !gc::hold(&mut tx, &st.blobs, &stored.key).await? {
        // Deleted by GC since the upload found it; the streamed body cannot be replayed.
        return Err(ApiError::Unavailable(
            "content was concurrently garbage-collected; retry the upload".into(),
        ));
    }
    db::insert_document(
        &mut tx,
        &NewDocument {
            id: doc_id,
            filename: p.filename.as_deref(),
            content_type: PDF,
            sha256: &stored.sha256,
            size_bytes: size,
            created_by: actor.as_deref(),
            labels: serde_json::Value::Object(labels),
        },
    )
    .await?;
    db::insert_artifact(
        &mut tx,
        &NewArtifact {
            id: art_id,
            document_id: doc_id,
            kind: "source",
            schema: SOURCE_SCHEMA,
            content_type: PDF,
            sha256: &stored.sha256,
            size_bytes: size,
            object_key: stored.key.as_ref(),
            produced_by: None,
            parent_artifact_id: None,
        },
    )
    .await?;
    db::insert_audit(
        &mut tx,
        doc_id,
        Some(art_id),
        "document.created",
        actor.as_deref(),
        json!({ "sha256": stored.sha256, "size_bytes": size, "filename": p.filename }),
    )
    .await?;
    let ev = Envelope::new(
        DOCUMENT_CREATED,
        SERVICE,
        doc_id,
        DocumentCreated {
            document_id: doc_id,
            artifact_id: art_id,
            sha256: stored.sha256.clone(),
            content_type: PDF.into(),
            size_bytes: size,
            filename: p.filename,
        },
    );
    db::enqueue(&mut tx, &ev.nats_subject(), &ev).await?;
    tx.commit().await?;
    st.relay_wake.notify_one();
    metrics::counter!("documents_created_total").increment(1);

    Ok((
        StatusCode::CREATED,
        Json(CreatedDocument {
            document_id: doc_id,
            artifact_id: art_id,
            sha256: stored.sha256,
            size_bytes: size,
        }),
    ))
}

#[derive(Deserialize, IntoParams)]
pub struct ListParams {
    /// `next_cursor` from the previous page.
    cursor: Option<Uuid>,
    /// 1..=1000, default 100.
    limit: Option<i64>,
    /// `asc` (default, oldest first) or `desc`.
    order: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DocumentPage {
    items: Vec<Document>,
    next_cursor: Option<Uuid>,
}

#[utoipa::path(get, path = "/v1/documents", params(ListParams), responses((status = 200, body = DocumentPage)))]
pub async fn list_documents(
    State(st): State<AppState>,
    Query(p): Query<ListParams>,
) -> ApiResult<Json<DocumentPage>> {
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let desc = match p.order.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(ApiError::BadRequest("order must be asc or desc".into())),
    };
    let items = db::list_documents(&st.db, p.cursor, limit, desc).await?;
    let next_cursor = (items.len() as i64 == limit)
        .then(|| items.last().map(|d| d.id))
        .flatten();
    Ok(Json(DocumentPage { items, next_cursor }))
}

#[derive(Serialize, ToSchema)]
pub struct DocumentDetail {
    #[serde(flatten)]
    document: Document,
    artifacts: Vec<Artifact>,
}

#[utoipa::path(get, path = "/v1/documents/{id}", responses((status = 200, body = DocumentDetail), (status = 404)))]
pub async fn get_document(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DocumentDetail>> {
    let document = db::get_document(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let artifacts = db::list_artifacts(&st.db, id).await?;
    Ok(Json(DocumentDetail {
        document,
        artifacts,
    }))
}

/// Download the source PDF. Supports `Range` and `If-None-Match`.
#[utoipa::path(
    get, path = "/v1/documents/{id}/source",
    responses((status = 200, content_type = "application/pdf"), (status = 206), (status = 304), (status = 404), (status = 416))
)]
pub async fn get_source(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let a = db::get_source_artifact(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    serve_blob(&st, &a, &headers).await
}

#[utoipa::path(get, path = "/v1/documents/{id}/audit", responses((status = 200, body = Vec<AuditEntry>), (status = 404)))]
pub async fn get_audit(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<AuditEntry>>> {
    let entries = db::list_audit(&st.db, id).await?;
    if entries.is_empty() && db::get_document(&st.db, id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(entries))
}
