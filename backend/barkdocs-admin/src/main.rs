//! Веб-консоль BarkDocs: выдача ключей MCP агентам, учёт запросов и управление
//! индексом документации. Права проверяет сама консоль; данные — в barkdocs-auth
//! и barkdocs-engine, своего хранилища нет.

mod client;
mod pages;
mod session;
mod views;

use std::{sync::LazyLock, time::Duration};

use axum::{
    Router,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use pages::{account, keys, libraries, overview, users};

#[derive(Clone)]
pub struct AppState {
    pub services: client::Services,
    /// Публичный адрес MCP-сервера для инструкций подключения.
    pub mcp_url: String,
    pub allow_registration: bool,
    pub cookie_secure: bool,
}

const CSS: &str = include_str!("../static/app.css");
const JS: &str = include_str!("../static/app.js");
const FAVICON: &str = include_str!("../static/favicon.svg");

/// Классы высоты и ширины полос: CSP запрещает инлайн-стили, поэтому доли — классами.
static FULL_CSS: LazyLock<String> = LazyLock::new(|| {
    let mut css = String::from(CSS);
    for i in 0..=100 {
        css.push_str(&format!(".h-{i}{{height:{i}%}}.w-{i}{{width:{i}%}}"));
    }
    css
});

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

/// Страницы содержат ключи и личную статистику: не кэшируются и не встраиваются в чужие сайты.
async fn security_headers(request: Request, next: Next) -> Response {
    let is_static = request.uri().path().starts_with("/static/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self' data:; \
             form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    if !is_static {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(overview::overview))
        .route("/login", get(account::login_page).post(account::login))
        .route(
            "/register",
            get(account::register_page).post(account::register),
        )
        .route("/logout", post(account::logout))
        .route("/account", get(account::account_page))
        .route("/account/password", post(account::change_password))
        .route("/keys", get(keys::keys_page).post(keys::create_key))
        .route("/keys/{id}/revoke", post(keys::revoke_key))
        .route("/libraries", get(libraries::list))
        .route("/libraries/new", get(libraries::new_form))
        .route("/libraries/new/auto", post(libraries::create_auto))
        .route("/libraries/new/custom", post(libraries::create_custom))
        .route("/library", get(libraries::detail))
        .route("/library/update", post(libraries::update))
        .route("/library/delete", post(libraries::delete))
        .route("/library/sync", post(libraries::sync))
        .route("/library/sources/add", post(libraries::add_source))
        .route("/library/sources/delete", post(libraries::delete_source))
        .route("/library/sources/sync", post(libraries::sync_source))
        .route("/users", get(users::list))
        .route("/users/{id}", get(users::detail))
        .route("/users/{id}/update", post(users::update))
        .route(
            "/static/app.css",
            get(|| async { asset("text/css; charset=utf-8", FULL_CSS.as_str()) }),
        )
        .route(
            "/static/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", JS) }),
        )
        .route(
            "/static/favicon.svg",
            get(|| async { asset("image/svg+xml", FAVICON) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .fallback(|| async {
            views::error_page(StatusCode::NOT_FOUND, "Страница не найдена.")
        })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            session::consume_flash,
        ))
        .layer(middleware::from_fn(session::csrf_guard))
        .layer(middleware::from_fn(security_headers))
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

    let state = AppState {
        services: client::Services {
            // Добавление библиотеки ищет её в реестрах синхронно и может занять десятки секунд.
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(180))
                .build()?,
            auth_url: std::env::var("AUTH_URL")?,
            engine_url: std::env::var("ENGINE_URL")?,
        },
        mcp_url: std::env::var("MCP_PUBLIC_URL")
            .unwrap_or_else(|_| "http://localhost:8080/mcp".into()),
        allow_registration: env_flag("ALLOW_REGISTRATION", true),
        cookie_secure: env_flag("COOKIE_SECURE", false),
    };
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8083".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("barkdocs-admin слушает {addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
