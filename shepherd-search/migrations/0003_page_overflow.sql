-- `include_pages` needs page matches only where the card's tsv does not cover the page
-- (`mapping::CARD_TSV_CHARS`). Existing rows default to false and stay searchable; a rebuild sets
-- the flag. On a populated table, create the index CONCURRENTLY by hand before deploying and
-- drop pages_tsv_idx afterwards; IF [NOT] EXISTS then skips both.

ALTER TABLE search.pages ADD COLUMN IF NOT EXISTS in_card boolean NOT NULL DEFAULT false;

CREATE INDEX IF NOT EXISTS pages_overflow_tsv_idx ON search.pages USING gin (tsv) WHERE NOT in_card;

-- Per-document page lookups (`matched_pages`) use the primary key.
DROP INDEX IF EXISTS search.pages_tsv_idx;
