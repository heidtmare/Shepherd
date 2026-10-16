-- Reference lookups for blob GC (`gc::Gc::sweep`).
-- On a populated table, create this CONCURRENTLY by hand before deploying; IF NOT EXISTS then skips it.

CREATE INDEX IF NOT EXISTS artifacts_object_key_idx ON storage.artifacts (object_key);
