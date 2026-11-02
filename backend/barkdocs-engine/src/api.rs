//! Внутренний HTTP API движка. Авторизацию выполняет barkdocs-web.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    AppState, db,
    discovery::{self, AddLibrary},
    manage, sync,
    usage::{self, Caller},
};

const MAX_QUERY_CHARS: usize = 1000;

pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn internal(message: &str) -> Self {
        tracing::error!("{message}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "внутренняя ошибка".into(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!("БД: {err}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "внутренняя ошибка".into(),
        )
    }
}

pub fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, message.into())
}

#[derive(Deserialize)]
struct ResolveParams {
    name: String,
}

/// Ищет в индексе, а если точного совпадения нет — в реестрах пакетов.
/// Найденное скачивается в фоне: клиент видит статус `indexing`.
async fn resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ResolveParams>,
) -> Result<Json<Vec<db::LibraryRow>>, ApiError> {
    let name = params.name.trim();
    if name.is_empty() {
        return Err(bad_request("пустое имя библиотеки"));
    }
    if discovery::valid_package_name(name) && !db::has_exact_match(&state.pool, name).await? {
        for candidate in discovery::search_registries(&state.http, name).await {
            if let Err(err) = discovery::register(&state, candidate, Some(name)).await {
                tracing::warn!("регистрация {name}: {err:#}");
            }
        }
    }
    let libraries = db::search_libraries(&state.pool, name).await?;
    usage::record(
        &state.pool,
        Caller::from_headers(&headers),
        usage::Record::resolve(name, libraries.len()),
    );
    Ok(Json(libraries))
}

#[derive(Deserialize)]
struct DocsParams {
    library_id: String,
    query: String,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct DocsResponse {
    library: db::LibraryRow,
    results: Vec<db::SearchHit>,
}

async fn docs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<DocsParams>,
) -> Result<Json<DocsResponse>, ApiError> {
    if params.query.trim().is_empty() || params.query.chars().count() > MAX_QUERY_CHARS {
        return Err(bad_request(format!(
            "query должен содержать от 1 до {MAX_QUERY_CHARS} символов"
        )));
    }
    let library = db::get_library(&state.pool, &params.library_id)
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "библиотека не найдена".into()))?;
    let limit = params.limit.unwrap_or(10).clamp(1, db::MAX_RESULTS);
    let results = db::search_chunks(&state.pool, &params.library_id, &params.query, limit).await?;
    usage::record(
        &state.pool,
        Caller::from_headers(&headers),
        usage::Record::docs(&library.library_id, &params.query, &results),
    );
    Ok(Json(DocsResponse { library, results }))
}

/// Добавляет библиотеку по ссылке (`url`) или имени пакета (`name`).
async fn add_library(
    State(state): State<AppState>,
    Json(request): Json<AddLibrary>,
) -> Result<(StatusCode, Json<Vec<db::LibraryRow>>), ApiError> {
    let registered = match (request.url, request.name) {
        (Some(url), None) => {
            let candidate = discovery::from_link(&url).map_err(|e| bad_request(e.to_string()))?;
            vec![discovery::register(&state, candidate, None).await]
        }
        (None, Some(name)) => {
            let name = name.trim();
            if !discovery::valid_package_name(name) {
                return Err(bad_request("недопустимое имя пакета"));
            }
            let mut results = Vec::new();
            for candidate in discovery::search_registries(&state.http, name).await {
                results.push(discovery::register(&state, candidate, Some(name)).await);
            }
            results
        }
        _ => return Err(bad_request("укажите ровно одно поле: url или name")),
    };

    let mut libraries = Vec::new();
    for result in registered {
        match result {
            Ok(Some(slug)) => libraries.extend(db::get_library(&state.pool, &slug).await?),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!("добавление библиотеки: {err:#}");
                return Err(ApiError(StatusCode::BAD_GATEWAY, format!("{err:#}")));
            }
        }
    }
    if libraries.is_empty() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "источники документации не найдены".into(),
        ));
    }
    Ok((StatusCode::CREATED, Json(libraries)))
}

#[derive(Deserialize)]
struct SyncRequest {
    library_id: String,
}

/// Внеплановая проверка: неизменившиеся источники всё равно не переиндексируются.
async fn sync_library(
    State(state): State<AppState>,
    Json(request): Json<SyncRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let sources = db::library_sources(&state.pool, &request.library_id).await?;
    if sources.is_empty() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "библиотека не найдена".into(),
        ));
    }
    for id in &sources {
        sync::spawn_sync(&state, *id, true);
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "sources": sources.len() })),
    ))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/libraries/resolve", get(resolve))
        .route("/libraries", post(add_library))
        .route("/sync", post(sync_library))
        .route("/docs", get(docs))
        .route("/stats", get(usage::stats))
        .merge(manage::router())
        .with_state(state)
}
