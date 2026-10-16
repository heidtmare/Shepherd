-- Storage schema. Tables are source of truth; artifacts and audit_log are append-only.

CREATE TABLE storage.documents (
    id              uuid PRIMARY KEY,
    source_filename text,
    content_type    text        NOT NULL,
    source_sha256   char(64)    NOT NULL,
    size_bytes      bigint      NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    created_by      text,
    labels          jsonb       NOT NULL DEFAULT '{}'
);
CREATE INDEX documents_sha256_idx ON storage.documents (source_sha256);

CREATE TABLE storage.artifacts (
    id                 uuid PRIMARY KEY,
    document_id        uuid        NOT NULL REFERENCES storage.documents (id),
    kind               text        NOT NULL CHECK (kind IN ('source', 'derived')),
    schema             text        NOT NULL,
    version            int         NOT NULL CHECK (version > 0),
    content_type       text        NOT NULL,
    sha256             char(64)    NOT NULL,
    size_bytes         bigint      NOT NULL,
    object_key         text        NOT NULL,
    produced_by        text,
    parent_artifact_id uuid REFERENCES storage.artifacts (id),
    created_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (document_id, schema, version)
);

CREATE TABLE storage.audit_log (
    id          bigserial PRIMARY KEY,
    document_id uuid        NOT NULL REFERENCES storage.documents (id),
    artifact_id uuid REFERENCES storage.artifacts (id),
    action      text        NOT NULL,
    actor       text,
    at          timestamptz NOT NULL DEFAULT now(),
    details     jsonb       NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_document_idx ON storage.audit_log (document_id, id);

CREATE TABLE storage.outbox (
    id           bigserial PRIMARY KEY,
    subject      text        NOT NULL,
    payload      jsonb       NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    published_at timestamptz
);
CREATE INDEX outbox_pending_idx ON storage.outbox (id) WHERE published_at IS NULL;

CREATE FUNCTION storage.reject_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME;
END
$$;

CREATE TRIGGER artifacts_append_only BEFORE UPDATE OR DELETE ON storage.artifacts
    FOR EACH ROW EXECUTE FUNCTION storage.reject_mutation();
CREATE TRIGGER audit_log_append_only BEFORE UPDATE OR DELETE ON storage.audit_log
    FOR EACH ROW EXECUTE FUNCTION storage.reject_mutation();
