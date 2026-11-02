//! Сервис авторизации: аккаунты веб-консоли, их сессии и API-ключи агентов.
//! В БД хранятся только хеши: sha256 для ключей и сессий, argon2id для паролей.

mod crypto;
mod error;
mod keys;
mod principal;
mod users;

use std::{str::FromStr, sync::Arc};

use axum::{
    Router,
    routing::{delete, get, post},
};
use sqlx::{
    Connection, PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub admin_key_hash: Vec<u8>,
    /// Логин администратора из окружения; занять его регистрацией нельзя.
    pub admin_login: Option<String>,
    pub allow_registration: bool,
    pub login_limiter: Arc<users::LoginLimiter>,
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/verify", post(keys::verify))
        .route("/keys", post(keys::create_key).get(keys::list_keys))
        .route("/keys/{id}", delete(keys::revoke_key))
        .route("/register", post(users::register))
        .route("/sessions", post(users::login).delete(users::logout))
        .route("/me", get(users::me))
        .route("/me/password", post(users::change_password))
        .route("/users", get(users::list_users))
        .route(
            "/users/{id}",
            get(users::get_user).patch(users::update_user),
        )
        .with_state(state)
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(value.trim(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
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
    let admin_password = std::env::var("ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());
    let admin_login = match &admin_password {
        Some(_) => {
            let raw = std::env::var("ADMIN_LOGIN").unwrap_or_else(|_| "admin".into());
            Some(
                users::normalize_login(&raw)
                    .map_err(|e| anyhow::anyhow!("ADMIN_LOGIN: {}", e.1))?,
            )
        }
        None => {
            tracing::warn!("ADMIN_PASSWORD не задан: войти в веб-консоль администратором нельзя");
            None
        }
    };

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
        admin_key_hash: crypto::hash_token(&admin_key),
        admin_login,
        allow_registration: env_flag("ALLOW_REGISTRATION", true),
        login_limiter: Arc::default(),
    };
    if let Some(password) = &admin_password {
        users::bootstrap_admin(&state, password).await?;
    }
    if !state.allow_registration {
        tracing::info!("регистрация новых пользователей отключена");
    }

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("barkdocs-auth слушает {addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
