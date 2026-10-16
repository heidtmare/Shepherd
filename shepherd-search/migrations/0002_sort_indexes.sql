-- Keyset indexes matching `backend::postgres::sort_expr`. Nullable sort columns are coalesced to a
-- direction-specific sentinel, so the plain column indexes cannot serve ORDER BY or the cursor.
-- On a populated table, create these CONCURRENTLY by hand before deploying; IF NOT EXISTS then skips them.

CREATE INDEX IF NOT EXISTS cards_received_desc_idx
    ON search.cards ((COALESCE(received_at, '-infinity'::timestamptz)) DESC, doc_id DESC);
CREATE INDEX IF NOT EXISTS cards_received_asc_idx
    ON search.cards ((COALESCE(received_at, 'infinity'::timestamptz)), doc_id);
CREATE INDEX IF NOT EXISTS cards_amount_desc_idx
    ON search.cards ((COALESCE(amount, '-Infinity'::numeric)) DESC, doc_id DESC);
CREATE INDEX IF NOT EXISTS cards_amount_asc_idx
    ON search.cards ((COALESCE(amount, 'Infinity'::numeric)), doc_id);
CREATE INDEX IF NOT EXISTS cards_filename_sort_idx
    ON search.cards ((COALESCE(filename, '')), doc_id);
