//! Сервис авторизации: выдача, отзыв и проверка API-ключей.
//! В БД хранится только sha256 ключа, сам ключ показывается один раз при создании.

use std::str::FromStr;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Connection, PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    admin_key_hash: Vec<u8>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Admin,
    User,
}

#[derive(Serialize)]
struct VerifyResponse {
    role: Role,
}

#[derive(Deserialize)]
struct CreateKeyRequest {
    name: String,
}

#[derive(Serialize)]
struct CreatedKey {
    id: i64,
    name: String,
    key: String,
}

#[derive(Serialize, sqlx::FromRow)]
struct KeyInfo {
    id: i64,
    name: String,
    created_at: String,
    revoked_at: Option<String>,
}

fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

fn generate_key() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(format!("bd_{}", hex::encode(bytes)))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn internal(err: impl std::fmt::Display) -> StatusCode {
    tracing::error!("{err}");
    StatusCode::INTERNAL_SERVER_ERROR
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    match bearer(headers) {
        Some(key) if hash_key(key) == state.admin_key_hash => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn verify(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<VerifyResponse>, StatusCode> {
    let key = bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let hash = hash_key(key);
    if hash == state.admin_key_hash {
        return Ok(Json(VerifyResponse { role: Role::Admin }));
    }
    let active: Option<i64> =
        sqlx::query_scalar("SELECT id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL")
            .bind(hash)
            .fetch_optional(&state.pool)
            .await
            .map_err(internal)?;
    match active {
        Some(_) => Ok(Json(VerifyResponse { role: Role::User })),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<CreatedKey>), StatusCode> {
    require_admin(&state, &headers)?;
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let key = generate_key().map_err(internal)?;
    let id: i64 =
        sqlx::query_scalar("INSERT INTO api_keys (name, key_hash) VALUES ($1, $2) RETURNING id")
            .bind(&name)
            .bind(hash_key(&key))
            .fetch_one(&state.pool)
            .await
            .map_err(internal)?;
    Ok((StatusCode::CREATED, Json(CreatedKey { id, name, key })))
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<KeyInfo>>, StatusCode> {
    require_admin(&state, &headers)?;
    let keys = sqlx::query_as(
        "SELECT id, name, created_at::text AS created_at, revoked_at::text AS revoked_at
         FROM api_keys ORDER BY id",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;
    Ok(Json(keys))
}

async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<StatusCode, StatusCode> {
    require_admin(&state, &headers)?;
    let result =
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(&state.pool)
            .await
            .map_err(internal)?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/verify", post(verify))
        .route("/keys", post(create_key).get(list_keys))
        .route("/keys/{id}", delete(revoke_key))
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let admin_key = std::env::var("ADMIN_KEY").unwrap_or_default();
    anyhow::ensure!(
        admin_key.len() >= 32,
        "ADMIN_KEY должен быть задан и содержать минимум 32 символа"
    );

    // У каждого сервиса своя схема: иначе миграции конфликтуют в общей _sqlx_migrations.
    let options = PgConnectOptions::from_str(&std::env::var("DATABASE_URL")?)?;
    let mut conn = PgConnection::connect_with(&options).await?;
    sqlx::query("CREATE SCHEMA IF NOT EXISTS auth")
        .execute(&mut conn)
        .await?;
    conn.close().await?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(options.options([("search_path", "auth")]))
        .await?;
    sqlx::migrate!().run(&pool).await?;

    let state = AppState {
        pool,
        admin_key_hash: hash_key(&admin_key),
    };
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("barkdocs-auth слушает {addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_unique_and_prefixed() {
        let a = generate_key().unwrap();
        let b = generate_key().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("bd_"));
        assert_eq!(a.len(), 3 + 64);
    }

    #[test]
    fn bearer_requires_exact_scheme() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&headers), Some("abc"));
    }
}
