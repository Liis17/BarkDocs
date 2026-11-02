//! Управление индексом из веб-консоли: список библиотек, ручное создание с указанными
//! источниками, правка, удаление и внеплановая синхронизация отдельных источников.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use url::Url;

use crate::{
    AppState,
    api::{ApiError, bad_request},
    db::{self, LibraryRow},
    discovery::site_slug,
    http::check_url,
    sources::github,
    sync,
};

const MAX_NAME_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 2000;
const MAX_ALIASES: usize = 20;
const MAX_SOURCES: usize = 20;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/libraries", get(list).patch(update).delete(remove))
        .route("/libraries/detail", get(detail))
        .route("/libraries/custom", post(create_custom))
        .route("/sources", post(add_source))
        .route("/sources/{id}", delete(remove_source))
        .route("/sources/{id}/sync", post(sync_source))
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "библиотека не найдена".into())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LibrarySummary {
    #[sqlx(flatten)]
    #[serde(flatten)]
    library: LibraryRow,
    sources: i64,
    requests: i64,
    last_requested_at: Option<String>,
}

#[derive(Deserialize)]
struct ListParams {
    q: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<LibrarySummary>>, ApiError> {
    let filter = params
        .q
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty());
    let sql = format!(
        "SELECT {columns},
                (SELECT count(*) FROM sources s WHERE s.library_id = l.id) AS sources,
                (SELECT count(*) FROM requests r WHERE r.library = l.slug AND r.kind = 'docs') AS requests,
                (SELECT to_char(max(r.at) AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI') FROM requests r
                  WHERE r.library = l.slug AND r.kind = 'docs') AS last_requested_at
         FROM libraries l
         WHERE $1::text IS NULL OR l.name ILIKE $1 OR l.slug ILIKE $1 OR l.description ILIKE $1
         ORDER BY requests DESC, l.stars DESC, l.slug",
        columns = db::LIBRARY_COLUMNS
    );
    let libraries = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(filter.map(|q| db::like_pattern(&q)))
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(libraries))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct SourceRow {
    id: i64,
    kind: String,
    url: String,
    version: Option<String>,
    last_checked_at: Option<String>,
    last_success_at: Option<String>,
    next_check_at: String,
    last_error: Option<String>,
    syncing: bool,
    chunks: i64,
}

#[derive(Serialize)]
struct LibraryDetail {
    library: LibraryRow,
    aliases: Vec<String>,
    created_at: String,
    sources: Vec<SourceRow>,
}

#[derive(Deserialize)]
struct LibraryParam {
    library_id: String,
}

const TIME: &str = "'YYYY-MM-DD HH24:MI'";

async fn detail(
    State(state): State<AppState>,
    Query(param): Query<LibraryParam>,
) -> Result<Json<LibraryDetail>, ApiError> {
    let library = db::get_library(&state.pool, &param.library_id)
        .await?
        .ok_or_else(not_found)?;
    let (aliases, created_at): (Vec<String>, String) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT aliases, to_char(created_at AT TIME ZONE 'UTC', {TIME}) FROM libraries WHERE slug = $1"
    )))
    .bind(&param.library_id)
    .fetch_one(&state.pool)
    .await?;
    let sql = format!(
        "SELECT s.id, s.kind, s.url, s.version,
                to_char(s.last_checked_at AT TIME ZONE 'UTC', {TIME}) AS last_checked_at,
                to_char(s.last_success_at AT TIME ZONE 'UTC', {TIME}) AS last_success_at,
                to_char(s.next_check_at AT TIME ZONE 'UTC', {TIME}) AS next_check_at,
                s.last_error,
                (s.sync_started_at IS NOT NULL AND s.sync_started_at > now() - interval '1 hour') AS syncing,
                (SELECT count(*) FROM chunks c WHERE c.source_id = s.id) AS chunks
         FROM sources s JOIN libraries l ON l.id = s.library_id
         WHERE l.slug = $1 ORDER BY s.id"
    );
    let sources = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&param.library_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(LibraryDetail {
        library,
        aliases,
        created_at,
        sources,
    }))
}

/// Тип источника определяется по ссылке, если не указан явно.
fn classify(url: &str) -> &'static str {
    if github::parse_repo(url).is_some() {
        return "github";
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    if path.ends_with("/llms.txt") || path.ends_with("/llms-full.txt") {
        "llms"
    } else {
        "site"
    }
}

/// Проверенный источник. У GitHub-репозитория ссылка приводится к виду `https://github.com/o/r`.
#[derive(Debug, PartialEq)]
struct ValidSource {
    kind: &'static str,
    url: String,
    repo: Option<(String, String)>,
}

fn validate_source(kind: Option<&str>, raw: &str) -> Result<ValidSource, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("пустая ссылка источника".into());
    }
    let kind = match kind.map(str::trim).filter(|k| !k.is_empty()) {
        None | Some("auto") => classify(raw),
        Some("github") => "github",
        Some("llms") => "llms",
        Some("site") => "site",
        Some(other) => return Err(format!("неизвестный тип источника: {other}")),
    };
    if kind == "github" {
        let (owner, repo) = github::parse_repo(raw)
            .ok_or_else(|| format!("не ссылка на GitHub-репозиторий: {raw}"))?;
        return Ok(ValidSource {
            kind,
            url: format!("https://github.com/{owner}/{repo}"),
            repo: Some((owner, repo)),
        });
    }
    let url = Url::parse(raw).map_err(|e| format!("{raw}: {e}"))?;
    check_url(&url).map_err(|e| format!("{raw}: {e}"))?;
    Ok(ValidSource {
        kind,
        url: url.to_string(),
        repo: None,
    })
}

/// Идентификатор библиотеки в стиле `/owner/repo`: строчные буквы, цифры и `.-_`.
fn normalize_slug(raw: &str) -> Result<String, String> {
    let slug = raw.trim().trim_end_matches('/').to_ascii_lowercase();
    let segments: Vec<&str> = slug.strip_prefix('/').unwrap_or("").split('/').collect();
    let valid = slug.len() <= 200
        && slug.starts_with('/')
        && segments.iter().all(|s| {
            !s.is_empty()
                && *s != "."
                && *s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        });
    if valid {
        Ok(slug)
    } else {
        Err("id библиотеки: вида /owner/name, латиница, цифры и . - _".into())
    }
}

fn normalize_aliases(raw: &[String]) -> Result<Vec<String>, String> {
    let mut aliases: Vec<String> = Vec::new();
    for alias in raw {
        let alias = alias.trim().to_ascii_lowercase();
        if alias.is_empty() {
            continue;
        }
        if alias.chars().count() > 100 {
            return Err("алиас длиннее 100 символов".into());
        }
        if !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    if aliases.len() > MAX_ALIASES {
        return Err(format!("не больше {MAX_ALIASES} алиасов"));
    }
    Ok(aliases)
}

fn check_text(value: &str, field: &str, max: usize) -> Result<(), ApiError> {
    if value.chars().count() > max {
        return Err(bad_request(format!("{field}: не длиннее {max} символов")));
    }
    Ok(())
}

#[derive(Deserialize)]
struct NewSource {
    kind: Option<String>,
    url: String,
}

#[derive(Deserialize)]
struct CustomLibrary {
    slug: Option<String>,
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    sources: Vec<NewSource>,
}

/// Метаданные репозитория: заодно проверяет, что он существует и доступен.
async fn repo_meta(
    state: &AppState,
    owner: &str,
    repo: &str,
) -> Result<github::RepoMeta, ApiError> {
    match github::repo_meta(state, owner, repo).await {
        Ok(Some(meta)) => Ok(meta),
        Ok(None) => Err(bad_request(format!("репозиторий {owner}/{repo} не найден"))),
        Err(err) => Err(ApiError(StatusCode::BAD_GATEWAY, format!("{err:#}"))),
    }
}

/// Создаёт библиотеку ровно с теми источниками, что указал администратор, без поиска в реестрах.
async fn create_custom(
    State(state): State<AppState>,
    Json(request): Json<CustomLibrary>,
) -> Result<(StatusCode, Json<LibraryRow>), ApiError> {
    if request.sources.is_empty() || request.sources.len() > MAX_SOURCES {
        return Err(bad_request(format!(
            "укажите от 1 до {MAX_SOURCES} источников"
        )));
    }
    let mut sources = Vec::new();
    for source in &request.sources {
        let valid = validate_source(source.kind.as_deref(), &source.url).map_err(bad_request)?;
        if !sources
            .iter()
            .any(|s: &ValidSource| s.kind == valid.kind && s.url == valid.url)
        {
            sources.push(valid);
        }
    }
    let aliases = normalize_aliases(&request.aliases).map_err(bad_request)?;
    let mut name = request.name.unwrap_or_default().trim().to_string();
    let mut description = request.description.unwrap_or_default().trim().to_string();
    check_text(&name, "название", MAX_NAME_CHARS)?;
    check_text(&description, "описание", MAX_DESCRIPTION_CHARS)?;

    let mut stars = 0;
    let mut derived_slug = None;
    for source in &mut sources {
        let Some((owner, repo)) = &source.repo else {
            continue;
        };
        let meta = repo_meta(&state, owner, repo).await?;
        source.url = format!("https://github.com/{}", meta.full_name);
        stars = stars.max(meta.stargazers_count);
        derived_slug.get_or_insert_with(|| format!("/{}", meta.full_name.to_ascii_lowercase()));
        if name.is_empty() {
            name = meta.name.clone();
        }
        if description.is_empty() {
            description = meta.description.clone().unwrap_or_default();
        }
    }
    // У llms-источника id и название берутся от каталога, а не от имени файла.
    let first_url = Url::parse(&sources[0].url).ok().map(|mut url| {
        if sources[0].kind == "llms" {
            let dir = url
                .path()
                .rsplit_once('/')
                .map_or("/", |(dir, _)| dir)
                .to_string();
            url.set_path(&dir);
        }
        url
    });
    if name.is_empty() {
        name = first_url
            .as_ref()
            .and_then(|u| u.host_str())
            .unwrap_or("library")
            .to_string();
    }
    let slug = match request
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => normalize_slug(raw).map_err(bad_request)?,
        None => derived_slug
            .or_else(|| first_url.as_ref().map(site_slug))
            .ok_or_else(|| bad_request("не удалось вывести id библиотеки, укажите его"))?,
    };

    let mut tx = state.pool.begin().await?;
    let library_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO libraries (slug, name, description, stars, aliases)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (slug) DO NOTHING RETURNING id",
    )
    .bind(&slug)
    .bind(&name)
    .bind(&description)
    .bind(stars)
    .bind(&aliases)
    .fetch_optional(&mut *tx)
    .await?;
    let library_id = library_id.ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            format!("библиотека {slug} уже существует: добавьте источники на её странице"),
        )
    })?;
    for source in &sources {
        sqlx::query("INSERT INTO sources (library_id, kind, url) VALUES ($1, $2, $3)")
            .bind(library_id)
            .bind(source.kind)
            .bind(&source.url)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    tracing::info!("создана библиотека {slug}, источников: {}", sources.len());
    sync::start_pending(&state, library_id).await?;
    let library = db::get_library(&state.pool, &slug)
        .await?
        .ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(library)))
}

#[derive(Deserialize)]
struct LibraryUpdate {
    library_id: String,
    name: Option<String>,
    description: Option<String>,
    aliases: Option<Vec<String>>,
}

async fn update(
    State(state): State<AppState>,
    Json(request): Json<LibraryUpdate>,
) -> Result<Json<LibraryRow>, ApiError> {
    let name = request.name.map(|n| n.trim().to_string());
    if name.as_deref() == Some("") {
        return Err(bad_request("название не может быть пустым"));
    }
    if let Some(name) = &name {
        check_text(name, "название", MAX_NAME_CHARS)?;
    }
    let description = request.description.map(|d| d.trim().to_string());
    if let Some(description) = &description {
        check_text(description, "описание", MAX_DESCRIPTION_CHARS)?;
    }
    let aliases = request
        .aliases
        .as_deref()
        .map(normalize_aliases)
        .transpose()
        .map_err(bad_request)?;
    let updated = sqlx::query(
        "UPDATE libraries SET name = coalesce($2, name), description = coalesce($3, description),
                              aliases = coalesce($4, aliases)
         WHERE slug = $1",
    )
    .bind(&request.library_id)
    .bind(name)
    .bind(description)
    .bind(aliases)
    .execute(&state.pool)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(not_found());
    }
    let library = db::get_library(&state.pool, &request.library_id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(library))
}

/// Удаляет библиотеку с источниками и индексом. Журнал запросов сохраняется.
async fn remove(
    State(state): State<AppState>,
    Query(param): Query<LibraryParam>,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query("DELETE FROM libraries WHERE slug = $1")
        .bind(&param.library_id)
        .execute(&state.pool)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(not_found());
    }
    tracing::info!("удалена библиотека {}", param.library_id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SourceRequest {
    library_id: String,
    kind: Option<String>,
    url: String,
}

async fn library_db_id(pool: &PgPool, slug: &str) -> Result<i64, ApiError> {
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM libraries WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await?;
    id.ok_or_else(not_found)
}

async fn add_source(
    State(state): State<AppState>,
    Json(request): Json<SourceRequest>,
) -> Result<StatusCode, ApiError> {
    let library_id = library_db_id(&state.pool, &request.library_id).await?;
    let mut source = validate_source(request.kind.as_deref(), &request.url).map_err(bad_request)?;
    if let Some((owner, repo)) = &source.repo {
        let meta = repo_meta(&state, owner, repo).await?;
        source.url = format!("https://github.com/{}", meta.full_name);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sources WHERE library_id = $1")
        .bind(library_id)
        .fetch_one(&state.pool)
        .await?;
    if count >= MAX_SOURCES as i64 {
        return Err(bad_request(format!("не больше {MAX_SOURCES} источников")));
    }
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO sources (library_id, kind, url) VALUES ($1, $2, $3)
         ON CONFLICT (library_id, kind, url) DO NOTHING RETURNING id",
    )
    .bind(library_id)
    .bind(source.kind)
    .bind(&source.url)
    .fetch_optional(&state.pool)
    .await?;
    let id = inserted
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "такой источник уже добавлен".into()))?;
    sync::spawn_sync(&state, id, true);
    Ok(StatusCode::CREATED)
}

async fn remove_source(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query("DELETE FROM sources WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, "источник не найден".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn sync_source(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM sources WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    exists.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "источник не найден".into()))?;
    sync::spawn_sync(&state, id, true);
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_is_detected_from_link() {
        let github = validate_source(None, "git+https://github.com/o/r.git").unwrap();
        assert_eq!(github.kind, "github");
        assert_eq!(github.url, "https://github.com/o/r");
        assert_eq!(
            validate_source(None, "https://x.dev/llms-full.txt")
                .unwrap()
                .kind,
            "llms"
        );
        assert_eq!(
            validate_source(Some("auto"), "https://x.dev/docs")
                .unwrap()
                .kind,
            "site"
        );
        assert_eq!(
            validate_source(Some("llms"), "https://x.dev/ai.txt")
                .unwrap()
                .kind,
            "llms"
        );
    }

    #[test]
    fn source_links_are_validated() {
        assert!(validate_source(None, "").is_err());
        assert!(validate_source(None, "http://127.0.0.1/docs").is_err());
        assert!(validate_source(Some("github"), "https://x.dev").is_err());
        assert!(validate_source(Some("ftp"), "https://x.dev").is_err());
        assert!(validate_source(None, "file:///etc/passwd").is_err());
    }

    #[test]
    fn slug_normalization() {
        assert_eq!(
            normalize_slug(" /Vercel/Next.js/ ").unwrap(),
            "/vercel/next.js"
        );
        assert_eq!(normalize_slug("/x.dev").unwrap(), "/x.dev");
        assert!(normalize_slug("vercel/next").is_err());
        assert!(normalize_slug("/a//b").is_err());
        assert!(normalize_slug("/a/../b").is_err());
        assert!(normalize_slug("/a b").is_err());
        assert!(normalize_slug("/").is_err());
    }

    #[test]
    fn aliases_are_deduplicated() {
        let raw = vec![
            "React".to_string(),
            " react ".into(),
            String::new(),
            "reactjs".into(),
        ];
        assert_eq!(normalize_aliases(&raw).unwrap(), ["react", "reactjs"]);
        let many: Vec<String> = (0..=MAX_ALIASES).map(|i| format!("a{i}")).collect();
        assert!(normalize_aliases(&many).is_err());
    }
}
