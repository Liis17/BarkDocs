//! Движок документации: находит библиотеки, скачивает их доки, индексирует в Postgres FTS
//! и раз в сутки актуализирует. Внутренний сервис: наружу его публикует только barkdocs-web.

mod api;
mod chunk;
mod db;
mod discovery;
mod http;
mod manage;
mod sources;
mod sync;
mod usage;

use std::{str::FromStr, sync::Arc};

use sqlx::{
    Connection, PgConnection, PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::sync::Semaphore;

/// Сколько источников синхронизируется одновременно.
const MAX_CONCURRENT_SYNCS: usize = 4;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub http: http::SafeClient,
    pub github_token: Option<String>,
    pub sync_slots: Arc<Semaphore>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // У каждого сервиса своя схема: иначе миграции конфликтуют в общей _sqlx_migrations.
    let options = PgConnectOptions::from_str(&std::env::var("DATABASE_URL")?)?;
    let mut conn = PgConnection::connect_with(&options).await?;
    sqlx::query("CREATE SCHEMA IF NOT EXISTS engine")
        .execute(&mut conn)
        .await?;
    conn.close().await?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(options.options([("search_path", "engine")]))
        .await?;
    sqlx::migrate!().run(&pool).await?;

    let github_token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
    if github_token.is_none() {
        tracing::warn!("GITHUB_TOKEN не задан: лимит GitHub API всего 60 запросов в час");
    }

    let state = AppState {
        pool,
        http: http::SafeClient::new()?,
        github_token,
        sync_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SYNCS)),
    };

    tokio::spawn(sync::scheduler(state.clone()));

    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8082".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("barkdocs-engine слушает {addr}");
    axum::serve(listener, api::router(state)).await?;
    Ok(())
}
