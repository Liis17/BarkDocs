CREATE TABLE api_keys (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT NOT NULL,
    key_hash   BYTEA NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ
);
