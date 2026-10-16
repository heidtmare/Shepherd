use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::json;
use shepherd_common::events::{ARTIFACT_CREATED, ArtifactCreated, Envelope};
use utoipa::IntoParams;
use uuid::Uuid;

use super::{AppState, actor, serve_blob};
use crate::{
    SERVICE,
    db::{self, Artifact, NewArtifact, SOURCE_SCHEMA},
    error::{ApiError, ApiResult},
    gc,
};

#[derive(Deserialize, IntoParams)]
pub struct CreateArtifactParams {
    /// Schema identifier, e.g. `intake.ir.v1`. `[a-z0-9._-]`, 1..=128 chars.
    schema: String,
    /// Producing process, e.g. `shepherd-intake@1.4.0`.
    produced_by: Option<String>,
    /// Artifact this one was derived from; must belong to the same document.
    parent_artifact_id: Option<Uuid>,
}

fn valid_schema(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s != SOURCE_SCHEMA
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

const JSON: &str = "application/json";
const PDF: &str = "application/pdf";

/// Media type of a derived artifact body from `Content-Type` (JSON when absent).
fn body_type(headers: &HeaderMap) -> Result<&'static str, ApiError> {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
    match ct.as_deref() {
        None | Some("application/json") => Ok(JSON),
        Some("application/pdf") => Ok(PDF),
        Some(other) => Err(ApiError::BadRequest(format!(
            "unsupported content type {other:?}; expected application/json or application/pdf"
        ))),
    }
}

/// Store a derived JSON document, or a derived PDF (`Content-Type: application/pdf`), such as a
/// generated letter. Versions are assigned per `(document, schema)`.
#[utoipa::path(
    post, path = "/v1/documents/{id}/artifacts", params(CreateArtifactParams),
    request_body(content = Object, content_type = "application/json"),
    responses((status = 201, body = Artifact), (status = 400), (status = 404), (status = 413))
)]
pub async fn create_artifact(
    State(st): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(p): Query<CreateArtifactParams>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Artifact>)> {
    if !valid_schema(&p.schema) {
        return Err(ApiError::BadRequest("invalid schema identifier".into()));
    }
    let content_type = body_type(&headers)?;
    if content_type == PDF {
        if !body.starts_with(b"%PDF-") {
            return Err(ApiError::BadRequest("body is not a PDF".into()));
        }
    } else {
        serde_json::from_slice::<serde::de::IgnoredAny>(&body)
            .map_err(|e| ApiError::BadRequest(format!("body is not valid JSON: {e}")))?;
    }
    let actor = actor(&headers);

    // Validate before uploading so rejected requests leave no blob behind.
    if db::get_document(&st.db, doc_id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    if let Some(parent) = p.parent_artifact_id
        && !db::artifact_belongs_to(&mut *st.db.acquire().await?, parent, doc_id).await?
    {
        return Err(ApiError::BadRequest(
            "parent_artifact_id does not belong to this document".into(),
        ));
    }

    let stored = st.blobs.put_bytes(body.clone()).await?;

    let mut tx = st.db.begin().await?;
    if !db::lock_document(&mut tx, doc_id).await? {
        return Err(ApiError::NotFound);
    }
    if stored.existed && !gc::hold(&mut tx, &st.blobs, &stored.key).await? {
        // Deleted by GC since the upload found it; GC is now blocked on this key.
        st.blobs.put_if_absent(&stored.key, body).await?;
    }
    if let Some(parent) = p.parent_artifact_id
        && !db::artifact_belongs_to(&mut tx, parent, doc_id).await?
    {
        return Err(ApiError::BadRequest(
            "parent_artifact_id does not belong to this document".into(),
        ));
    }
    let art = db::insert_artifact(
        &mut tx,
        &NewArtifact {
            id: Uuid::now_v7(),
            document_id: doc_id,
            kind: "derived",
            schema: &p.schema,
            content_type,
            sha256: &stored.sha256,
            size_bytes: stored.size as i64,
            object_key: stored.key.as_ref(),
            produced_by: p.produced_by.as_deref(),
            parent_artifact_id: p.parent_artifact_id,
        },
    )
    .await?;
    db::insert_audit(
        &mut tx,
        doc_id,
        Some(art.id),
        "artifact.created",
        actor.as_deref(),
        json!({ "schema": art.schema, "version": art.version, "sha256": art.sha256, "produced_by": art.produced_by }),
    )
    .await?;
    let ev = Envelope::new(
        ARTIFACT_CREATED,
        SERVICE,
        doc_id,
        ArtifactCreated {
            document_id: doc_id,
            artifact_id: art.id,
            schema: art.schema.clone(),
            version: art.version,
            sha256: art.sha256.clone(),
        },
    );
    db::enqueue(&mut tx, &ev.nats_subject(), &ev).await?;
    tx.commit().await?;
    st.relay_wake.notify_one();
    metrics::counter!("artifacts_created_total").increment(1);

    Ok((StatusCode::CREATED, Json(art)))
}

#[utoipa::path(get, path = "/v1/artifacts/{id}", responses((status = 200, body = Artifact), (status = 404)))]
pub async fn get_artifact(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Artifact>> {
    Ok(Json(
        db::get_artifact(&st.db, id)
            .await?
            .ok_or(ApiError::NotFound)?,
    ))
}

/// Raw artifact bytes (PDF or JSON). Supports `Range` and `If-None-Match`.
#[utoipa::path(get, path = "/v1/artifacts/{id}/content", responses((status = 200), (status = 206), (status = 304), (status = 404)))]
pub async fn get_artifact_content(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let a = db::get_artifact(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    serve_blob(&st, &a, &headers).await
}

#[cfg(test)]
mod tests {
    use super::{body_type, valid_schema};
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn body_types() {
        let mut h = HeaderMap::new();
        assert_eq!(body_type(&h).unwrap(), "application/json");
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert_eq!(body_type(&h).unwrap(), "application/json");
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/pdf"),
        );
        assert_eq!(body_type(&h).unwrap(), "application/pdf");
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(body_type(&h).is_err());
    }

    #[test]
    fn schema_identifiers() {
        assert!(valid_schema("intake.ir.v1"));
        assert!(valid_schema("ocr-text_v2"));
        assert!(!valid_schema(""));
        assert!(!valid_schema("source"));
        assert!(!valid_schema("Intake"));
        assert!(!valid_schema("a/b"));
        assert!(!valid_schema(&"a".repeat(129)));
    }
}
