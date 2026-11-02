-- Журнал запросов к документации: из него считаются все счётчики веб-консоли.
-- Библиотека хранится по slug без внешнего ключа, чтобы статистика пережила её удаление.
CREATE TABLE requests (
    id         BIGSERIAL PRIMARY KEY,
    at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id    BIGINT,
    key_id     BIGINT,
    channel    TEXT NOT NULL CHECK (channel IN ('mcp', 'api', 'web')),
    kind       TEXT NOT NULL CHECK (kind IN ('resolve', 'docs')),
    library    TEXT,
    query      TEXT NOT NULL,
    results    INT NOT NULL,
    chars      BIGINT NOT NULL,
    urls       TEXT[] NOT NULL DEFAULT '{}'
);

CREATE INDEX requests_at_idx ON requests (at);
CREATE INDEX requests_user_at_idx ON requests (user_id, at);
CREATE INDEX requests_key_at_idx ON requests (key_id, at);
CREATE INDEX requests_library_at_idx ON requests (library, at);
