# shepherd-search

Card-catalog search over storage documents, using Postgres full-text search in the shared database (schema `search`).

- Input: the durable consumer `search-indexer`, declared in `shepherd-stream/config/topology.toml` and filtered on `shepherd.storage.>`. On each event, the indexer re-reads the document's current state from `storage.*` and rebuilds its card, so redelivery and reordering are harmless. Messages are acked after the write commits. Failures are nak'd with exponential backoff. After the consumer's `max_deliver`, messages go to `shepherd.dlq.search-indexer`.
- Output: each applied write publishes `shepherd.search.card.indexed.<doc_id>`.
- Projection: `search.cards` (one row per document) and `search.pages` (per-page text for long documents). Both are derived, and can be rebuilt with `POST /admin/rebuild`.
- Mapping: `config/mapping.toml` maps each derived schema's JSONPath expressions to content, pages, typed columns and facets. Typed columns are declared in `[columns]` (see Columns). The latest JSON of each schema is stored in `fields` (jsonb) for `contains` filters, except schemas mapped with `fields = false` (`intake.ir.v1`, whose text is already in `content` and `search.pages`). Schemas listed in top-level `ignore` are not fetched at all and contribute only their `schema/<name>` facet.
- Artifact reads: artifacts are immutable, so parsed bodies are cached by artifact id (`ARTIFACT_CACHE_MB`). A re-index fetches only artifacts created since the document was last indexed by this process.
- Engine seam: `backend::SearchBackend`. `backend::postgres` is the only implementation.

## Query API

`POST /v1/search`

```json
{
  "q": "water damage -hail",
  "filters": [
    {"op": "eq", "field": "status", "value": "pending_review"},
    {"op": "in", "field": "schema", "values": ["intake.ir.v1"]},
    {"op": "range", "field": "amount", "gte": 1000, "lt": 5000},
    {"op": "contains", "value": {"inference.claim.v1": {"totals": {"currency": "USD"}}}},
    {"op": "facet", "value": "type/insurance_claim"}
  ],
  "facets": ["type", "status"],
  "sort": {"field": "relevance", "dir": "desc"},
  "limit": 20,
  "cursor": null,
  "include_pages": true
}
```

- `q` uses `websearch_to_tsquery` syntax (`"phrase"`, `-term`, `or`) and also matches filenames by trigram similarity. Results are ranked by `ts_rank_cd` plus a small weight for filename similarity.
- Filter fields are `filename`, `created_at`, `updated_at`, `schema` and the declared columns (`status`, `amount`, `received_at` in the shipped mapping). Any other field returns 400.
- Sort fields are `relevance` (the default when `q` is set), `updated_at` (the default otherwise), `created_at`, `filename` and declared columns with `sort = true`. Null values sort last.
- Hit summaries carry every declared column by name (null when unset).
- Pagination is keyset. Pass `next_cursor` back with the same sort; a cursor used with a different sort returns 400.
- `facet_counts` are computed over up to `FACET_WINDOW` matches, up to 50 values per facet. `facet_counts_exact` is false when the match set is larger; the counts are then lower bounds taken in scan order. Counts are cached per match set for `FACET_CACHE_TTL_MS`, so later pages and repeated queries reuse them.
- `highlight` is HTML-escaped, with matches wrapped in `<mark>`. It is taken from the first 100k characters of content.

Other routes:
- `GET /v1/cards/{id}`
- `POST|GET /admin/rebuild`: requires `Authorization: Bearer $ADMIN_TOKEN`. These routes are not mounted when `ADMIN_TOKEN` is unset.

## Columns

`mapping.toml` declares typed card columns:

```toml
[columns]
status = { type = "text" }
amount = { type = "numeric", sort = true }
```

`type` is `text`, `numeric` or `timestamp`. `sort = true` is allowed for numeric and timestamp. Names are lowercase identifiers of up to 40 characters, excluding built-in names. Schemas fill columns through `[schema.columns]`.

- Startup (`columns::sync`) adds missing columns to `search.cards`. This is a metadata-only change. Startup fails if an existing column has a different type.
- A background task (`columns::ensure_indexes`) then creates, `CONCURRENTLY`, a btree `cards_col_<name>_idx` for each column. For sortable columns it also creates keyset indexes `cards_col_<name>_{asc,desc}_idx`. One process builds at a time (advisory lock), and invalid indexes from an interrupted build are replaced. Queries work before the indexes exist, only slower.
- Existing cards get values for a new column after a rebuild.
- Removing a column from the mapping stops filling it and removes it from the API. The column and its indexes are not dropped.

## Rebuild

`POST /admin/rebuild` records a rebuild in `search.rebuilds` and splits the document id space into up to `REBUILD_SHARDS` shards at boundaries sampled from `storage.documents` (`search.rebuild_shards`). Every search process runs a worker, whether or not it serves the admin routes. Each worker claims one shard at a time with `FOR UPDATE SKIP LOCKED`, re-indexes it in pages of 200 with `REBUILD_CONCURRENCY` documents in flight, and after each page stores the cursor and a heartbeat.

- Restart: on shutdown a worker releases its shard. A shard whose heartbeat is older than 120 s is reclaimed. Either way the next worker resumes from the cursor, so at most one page is repeated.
- Scale-out: add search processes. `REBUILD_CONCURRENCY=0` excludes a process.
- Completion: the worker that finds every shard done deletes cards stamped more than 60 s before the rebuild started (documents no longer in storage) and marks the rebuild `succeeded`, in one transaction. If any document failed, orphan removal is skipped and the rebuild is marked `failed`.
- `GET /admin/rebuild` returns the latest rebuild: `state`, `indexed`, `failed`, `removed`, `shards`, `shards_done`.
- `/healthz`, `/readyz`, `/metrics`, `/openapi.json`

## Configuration (env)

| Var | Default | |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8082` | |
| `DATABASE_URL` | required | Role owns schema `search` and needs SELECT on `storage.documents` and `storage.artifacts`. |
| `READ_DATABASE_URL` | `DATABASE_URL` | Query pool. Point it at a replica to isolate query load. |
| `NATS_URL` | required | |
| `MAPPING_PATH` | `config/mapping.toml` | Validated at startup. |
| `CONTENT_SOURCE` | `blob` | `blob` reads artifact JSON from the object store (uses the same `BLOB_*` vars as storage). `http` fetches it from `STORAGE_URL`. |
| `ADMIN_TOKEN` | unset | |
| `CONSUMER_BATCH`, `CONSUMER_CONCURRENCY` | 256, 16 | Retry limits come from the topology. |
| `QUERY_TIMEOUT_MS` | 2000 | `statement_timeout` on the query pool. A timeout returns 503. |
| `RANK_WINDOW` | 2000 | Number of matches scored under relevance sort. See Limits. |
| `FACET_WINDOW` | 100000 | Matches aggregated for facet counts. See Limits. |
| `FACET_CACHE_TTL_MS` | 10000 | Per-process facet count cache; 0 disables. |
| `QUERY_WORK_MEM` | `32MB` | `work_mem` on the query pool. Prevents lossy bitmap scans on broad matches. |
| `DB_MAX_CONNECTIONS` | 20 | |
| `REBUILD_SHARDS`, `REBUILD_CONCURRENCY` | 32, 8 | See Rebuild. |
| `ARTIFACT_CACHE_MB` | 256 | Parsed artifact bodies, bounded by serialized size, oldest evicted first. Bodies over 1/8 of the budget are not cached. 0 disables. |

## Limits

- Relevance is computed over a bounded window. `ts_rank_cd` has no index support and costs about 20 µs per matching row, so ranking every match does not scale: a term matching 200k cards takes about 4 s. Under relevance sort, only the `RANK_WINDOW` most recently updated matches are scored, which takes about 80 ms at 200k matches. Filters and explicit sorts are unaffected. Queries that match fewer than `RANK_WINDOW` cards are ranked exactly. If full-corpus BM25 ranking becomes a requirement, swap in a Tantivy backend or ParadeDB.

- `cards.content` is stored truncated to the 300k characters that `cards.tsv` indexes (tsvector has a 1 MB cap); full page text lives in `search.pages`. Use `include_pages` to match text beyond that. Pages that lie wholly within that prefix are flagged `in_card` and skipped for matching (partial index `pages_overflow_tsv_idx`), so only long documents contribute page matches. Page matches are capped at 100k rows per query; beyond that, overflow-only matches may be missed. Pages written before `0003_page_overflow.sql` default to overflow until rebuilt.
- Facet counts are bounded by `FACET_WINDOW`. Unbounded counting is linear in matches: 1.67M matching cards (5M total) take about 1.5 s, against about 110 ms at the default window. Within the window counts are exact; beyond it they are lower bounds from an arbitrary (scan-order) subset, not a uniform sample. Exact totals for broad sets need precomputed counters.
- The `schema` filter is evaluated against the reserved `schema/<name>` facets (GIN-indexed).
- The `filename` sort key uses an expression index (`migrations/0002_sort_indexes.sql`). Column indexes are managed as described under Columns.
- Changing `mapping.toml` affects only documents indexed afterwards. Run a rebuild to apply it to existing cards.
