-- Аккаунты веб-консоли. Ключ получает владельца; старые ключи без владельца остаются рабочими.
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    login         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role          TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('admin', 'user')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at   TIMESTAMPTZ
);

CREATE TABLE sessions (
    token_hash BYTEA PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX sessions_user_id_idx ON sessions (user_id);

ALTER TABLE api_keys
    ADD COLUMN user_id      BIGINT REFERENCES users (id) ON DELETE CASCADE,
    ADD COLUMN last_used_at TIMESTAMPTZ;

CREATE INDEX api_keys_user_id_idx ON api_keys (user_id);
