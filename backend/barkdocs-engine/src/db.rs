//! Запросы к Postgres.

use serde::Serialize;
use sqlx::PgPool;

use crate::chunk::Chunk;

const INSERT_BATCH: usize = 1000;
pub const MAX_RESULTS: i64 = 50;

pub struct NewLibrary {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub stars: i64,
    pub alias: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LibraryRow {
    pub library_id: String,
    pub name: String,
    pub description: String,
    pub stars: i64,
    pub chunks: i64,
    /// ready — есть что искать; indexing — идёт первая загрузка; failed — загрузить не удалось.
    pub status: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct SearchHit {
    pub url: String,
    pub heading: String,
    pub content: String,
    pub rank: f32,
}

#[derive(sqlx::FromRow)]
pub struct ClaimedSource {
    pub kind: String,
    pub url: String,
    pub version: Option<String>,
}

pub const LIBRARY_COLUMNS: &str = "
    l.slug AS library_id, l.name, l.description, l.stars,
    (SELECT count(*) FROM chunks c JOIN sources s ON s.id = c.source_id WHERE s.library_id = l.id)
        AS chunks,
    CASE
        WHEN EXISTS (SELECT 1 FROM chunks c JOIN sources s ON s.id = c.source_id WHERE s.library_id = l.id)
            THEN 'ready'
        WHEN EXISTS (SELECT 1 FROM sources s WHERE s.library_id = l.id
                     AND (s.sync_started_at IS NOT NULL OR s.last_checked_at IS NULL))
            THEN 'indexing'
        ELSE 'failed'
    END AS status,
    (SELECT max(s.last_success_at)::text FROM sources s WHERE s.library_id = l.id) AS last_synced_at,
    (SELECT s.last_error FROM sources s WHERE s.library_id = l.id AND s.last_error IS NOT NULL LIMIT 1)
        AS last_error";

pub async fn get_library(pool: &PgPool, slug: &str) -> sqlx::Result<Option<LibraryRow>> {
    let sql = format!("SELECT {LIBRARY_COLUMNS} FROM libraries l WHERE l.slug = $1");
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(slug)
        .fetch_optional(pool)
        .await
}

pub fn like_pattern(name: &str) -> String {
    let escaped = name
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Точные совпадения по имени и алиасу идут первыми, затем популярные.
pub async fn search_libraries(pool: &PgPool, name: &str) -> sqlx::Result<Vec<LibraryRow>> {
    let sql = format!(
        "SELECT {LIBRARY_COLUMNS} FROM libraries l
         WHERE l.name ILIKE $1 OR l.slug ILIKE $1 OR lower($2) = ANY (l.aliases)
         ORDER BY (lower(l.name) = lower($2) OR lower($2) = ANY (l.aliases)) DESC, l.stars DESC
         LIMIT 10"
    );
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(like_pattern(name))
        .bind(name)
        .fetch_all(pool)
        .await
}

pub async fn has_exact_match(pool: &PgPool, name: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM libraries
                        WHERE lower(name) = lower($1) OR lower($1) = ANY (aliases))",
    )
    .bind(name)
    .fetch_one(pool)
    .await
}

/// Слова запроса объединяются через OR: при AND вопрос естественным языком почти ничего не находит.
/// Лучшие совпадения поднимает ранжирование. Фразы в кавычках остаются фразами.
pub async fn search_chunks(
    pool: &PgPool,
    slug: &str,
    query: &str,
    limit: i64,
) -> sqlx::Result<Vec<SearchHit>> {
    sqlx::query_as(
        "SELECT c.url, c.heading, c.content, ts_rank_cd(c.tsv, q) AS rank
         FROM libraries l
         JOIN sources s ON s.library_id = l.id
         JOIN chunks c ON c.source_id = s.id
         CROSS JOIN to_tsquery('english',
             replace(websearch_to_tsquery('english', $2)::text, ' & ', ' | ')) AS q
         WHERE l.slug = $1 AND c.tsv @@ q
         ORDER BY rank DESC, c.id
         LIMIT $3",
    )
    .bind(slug)
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn upsert_library(
    pool: &PgPool,
    library: &NewLibrary,
    sources: &[(&str, String)],
) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    let library_id: i64 = sqlx::query_scalar(
        "INSERT INTO libraries (slug, name, description, stars, aliases)
         VALUES ($1, $2, $3, $4, CASE WHEN $5::text IS NULL THEN '{}'::text[] ELSE ARRAY[$5::text] END)
         ON CONFLICT (slug) DO UPDATE SET
             description = EXCLUDED.description,
             stars = EXCLUDED.stars,
             aliases = CASE
                 WHEN $5 IS NULL OR $5 = ANY (libraries.aliases) THEN libraries.aliases
                 ELSE array_append(libraries.aliases, $5)
             END
         RETURNING id",
    )
    .bind(&library.slug)
    .bind(&library.name)
    .bind(&library.description)
    .bind(library.stars)
    .bind(&library.alias)
    .fetch_one(&mut *tx)
    .await?;
    for (kind, url) in sources {
        sqlx::query(
            "INSERT INTO sources (library_id, kind, url) VALUES ($1, $2, $3)
             ON CONFLICT (library_id, kind, url) DO NOTHING",
        )
        .bind(library_id)
        .bind(kind)
        .bind(url)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(library_id)
}

/// Источники, которым подошёл срок проверки.
pub async fn due_sources(pool: &PgPool) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar("SELECT id FROM sources WHERE next_check_at <= now()")
        .fetch_all(pool)
        .await
}

pub async fn pending_sources(pool: &PgPool, library_id: i64) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar("SELECT id FROM sources WHERE library_id = $1 AND last_checked_at IS NULL")
        .bind(library_id)
        .fetch_all(pool)
        .await
}

pub async fn library_sources(pool: &PgPool, slug: &str) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT s.id FROM sources s JOIN libraries l ON l.id = s.library_id WHERE l.slug = $1",
    )
    .bind(slug)
    .fetch_all(pool)
    .await
}

/// Атомарно занимает источник, чтобы он не синхронизировался дважды одновременно.
/// Метка старше часа считается следом упавшего процесса.
pub async fn claim_source(
    pool: &PgPool,
    source_id: i64,
    force: bool,
) -> sqlx::Result<Option<ClaimedSource>> {
    sqlx::query_as(
        "UPDATE sources SET sync_started_at = now()
         WHERE id = $1
           AND (sync_started_at IS NULL OR sync_started_at < now() - interval '1 hour')
           AND ($2 OR next_check_at <= now())
         RETURNING kind, url, version",
    )
    .bind(source_id)
    .bind(force)
    .fetch_optional(pool)
    .await
}

pub async fn mark_unchanged(pool: &PgPool, source_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE sources SET sync_started_at = NULL, last_checked_at = now(),
                            next_check_at = now() + interval '24 hours',
                            last_success_at = now(), last_error = NULL
         WHERE id = $1",
    )
    .bind(source_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_failed(
    pool: &PgPool,
    source_id: i64,
    error: &str,
    retry_after_hours: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE sources SET sync_started_at = NULL, last_checked_at = now(), last_error = $2,
                            next_check_at = now() + make_interval(hours => $3)
         WHERE id = $1",
    )
    .bind(source_id)
    .bind(error)
    .bind(retry_after_hours)
    .execute(pool)
    .await?;
    Ok(())
}

/// Заменяет чанки источника одной транзакцией: поиск не увидит частичный индекс.
pub async fn replace_chunks(
    pool: &PgPool,
    source_id: i64,
    version: &str,
    chunks: &[(String, Chunk)],
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM chunks WHERE source_id = $1")
        .bind(source_id)
        .execute(&mut *tx)
        .await?;
    for batch in chunks.chunks(INSERT_BATCH) {
        let urls: Vec<&str> = batch.iter().map(|(url, _)| url.as_str()).collect();
        let headings: Vec<&str> = batch.iter().map(|(_, c)| c.heading.as_str()).collect();
        let contents: Vec<&str> = batch.iter().map(|(_, c)| c.content.as_str()).collect();
        sqlx::query(
            "INSERT INTO chunks (source_id, url, heading, content)
             SELECT $1, * FROM UNNEST($2::text[], $3::text[], $4::text[])",
        )
        .bind(source_id)
        .bind(&urls)
        .bind(&headings)
        .bind(&contents)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE sources SET version = $2, sync_started_at = NULL, last_checked_at = now(),
                            next_check_at = now() + interval '24 hours',
                            last_success_at = now(), last_error = NULL
         WHERE id = $1",
    )
    .bind(source_id)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("a%b_c\\d"), "%a\\%b\\_c\\\\d%");
    }
}
