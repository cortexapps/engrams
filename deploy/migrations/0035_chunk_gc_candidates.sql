-- ADR 0016 Phase C: chunk-GC candidate table.
--
-- The sweep upserts every BlobStorage chunk that isn't in the pin set
-- into this table. The promote-pass runs separately, deleting rows
-- (and the underlying chunks from BlobStorage) where
-- `first_seen_at < now() - grace_period` — default 24h. Re-seeing a
-- candidate refreshes `last_seen_at` for diagnostic ("how long has
-- this chunk been hanging around?") but does NOT extend the grace
-- window: `first_seen_at` is sticky on conflict.
--
-- The grace window is the third safety net (alongside the live-set
-- union in §"Phase C — chunk-GC pin-set redesign" and the
-- `chunk_generation` mid-sweep barrier from migration 0034). If a
-- chunk gets pinned between sweep N (where it was a candidate) and
-- sweep N+1 (where it isn't), the promote-pass at sweep N+1 simply
-- doesn't see the row anymore — no deletion happens.
--
-- Storing the hash as BYTEA (32 bytes for sha256) keeps the row
-- compact and the primary key narrow. Coord-side helpers go through
-- `ChunkHash::as_bytes()` so the wire shape across the trait boundary
-- is `[u8; 32]`, not a hex string.

CREATE TABLE IF NOT EXISTS chunk_gc_candidates (
    content_hash  BYTEA       PRIMARY KEY,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The promote-pass query is
--   SELECT content_hash FROM chunk_gc_candidates
--    WHERE first_seen_at < $cutoff
--    ORDER BY first_seen_at
--    LIMIT $batch;
-- so we index `first_seen_at` to cover both the predicate and the
-- ORDER BY. The PK already covers point upserts + batch deletes by
-- hash list.
CREATE INDEX IF NOT EXISTS idx_chunk_gc_candidates_first_seen
    ON chunk_gc_candidates (first_seen_at);
