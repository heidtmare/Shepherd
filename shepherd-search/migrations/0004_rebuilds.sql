-- Durable rebuild progress (`rebuild.rs`). The document id space is split into shards at sampled
-- boundaries; any search process claims a shard, advances its cursor per page and heartbeats.
-- A shard whose heartbeat is stale is reclaimed and resumes from its cursor.

CREATE TABLE search.rebuilds (
    id          bigserial   PRIMARY KEY,
    state       text        NOT NULL CHECK (state IN ('running', 'succeeded', 'failed')),
    started_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    removed     bigint      NOT NULL DEFAULT 0,
    error       text
);
CREATE UNIQUE INDEX rebuilds_one_running ON search.rebuilds ((true)) WHERE state = 'running';

CREATE TABLE search.rebuild_shards (
    rebuild_id   bigint  NOT NULL REFERENCES search.rebuilds (id) ON DELETE CASCADE,
    shard        int     NOT NULL,
    upto         uuid,                          -- inclusive upper bound; NULL = unbounded
    cursor       uuid,                          -- exclusive lower bound, advanced per page; NULL = unbounded
    done         boolean NOT NULL DEFAULT false,
    indexed      bigint  NOT NULL DEFAULT 0,
    failed       bigint  NOT NULL DEFAULT 0,
    owner        uuid,
    heartbeat_at timestamptz,
    PRIMARY KEY (rebuild_id, shard)
);
