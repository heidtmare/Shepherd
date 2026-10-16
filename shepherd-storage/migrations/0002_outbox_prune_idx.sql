-- Serves `outbox::Relay::prune`, which deletes published rows in batches by `published_at`.
-- On a populated table, create this CONCURRENTLY by hand before deploying; IF NOT EXISTS then skips it.

CREATE INDEX IF NOT EXISTS outbox_published_idx
    ON storage.outbox (published_at) WHERE published_at IS NOT NULL;
