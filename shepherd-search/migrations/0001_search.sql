-- Search projection. Derived from the storage schema; safe to truncate and rebuild.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE search.cards (
    doc_id      uuid PRIMARY KEY,
    filename    text,
    content     text        NOT NULL DEFAULT '',
    fields      jsonb       NOT NULL DEFAULT '{}',  -- latest derived JSON keyed by schema
    status      text,
    amount      numeric,
    received_at timestamptz,
    facets      text[]      NOT NULL DEFAULT '{}',  -- "name/value"
    schemas     text[]      NOT NULL DEFAULT '{}',
    source_seq  bigint      NOT NULL DEFAULT 0,     -- highest JetStream sequence applied
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    indexed_at  timestamptz NOT NULL,
    -- Content is truncated to stay under the 1 MB tsvector limit; search.pages covers full text.
    tsv tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(filename, '')), 'A') ||
        setweight(to_tsvector('english', left(content, 300000)), 'B')
    ) STORED
);

CREATE TABLE search.pages (
    doc_id  uuid NOT NULL REFERENCES search.cards (doc_id) ON DELETE CASCADE,
    page_no int  NOT NULL,
    text    text NOT NULL,
    tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', left(text, 300000))) STORED,
    PRIMARY KEY (doc_id, page_no)
);

CREATE INDEX cards_tsv_idx         ON search.cards USING gin (tsv);
CREATE INDEX cards_fields_idx      ON search.cards USING gin (fields jsonb_path_ops);
CREATE INDEX cards_filename_trgm   ON search.cards USING gin (filename gin_trgm_ops);
CREATE INDEX cards_facets_idx      ON search.cards USING gin (facets);
CREATE INDEX cards_updated_idx     ON search.cards (updated_at DESC, doc_id DESC);
CREATE INDEX cards_created_idx     ON search.cards (created_at DESC, doc_id DESC);
CREATE INDEX cards_status_idx      ON search.cards (status);
CREATE INDEX cards_amount_idx      ON search.cards (amount);
CREATE INDEX cards_received_idx    ON search.cards (received_at);
CREATE INDEX pages_tsv_idx         ON search.pages USING gin (tsv);
