CREATE TABLE libraries (
    id          BIGSERIAL PRIMARY KEY,
    slug        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    stars       BIGINT NOT NULL DEFAULT 0,
    aliases     TEXT[] NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sources (
    id              BIGSERIAL PRIMARY KEY,
    library_id      BIGINT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK (kind IN ('github', 'llms', 'site')),
    url             TEXT NOT NULL,
    version         TEXT,
    sync_started_at TIMESTAMPTZ,
    last_checked_at TIMESTAMPTZ,
    next_check_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_success_at TIMESTAMPTZ,
    last_error      TEXT,
    UNIQUE (library_id, kind, url)
);

CREATE TABLE chunks (
    id        BIGSERIAL PRIMARY KEY,
    source_id BIGINT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    url       TEXT NOT NULL,
    heading   TEXT NOT NULL,
    content   TEXT NOT NULL,
    tsv       TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', heading), 'A') || to_tsvector('english', content)
    ) STORED
);

CREATE INDEX chunks_source_id_idx ON chunks (source_id);
CREATE INDEX chunks_tsv_idx ON chunks USING GIN (tsv);
