//! Публичный шлюз: проверяет API-ключ через barkdocs-auth и предоставляет
//! HTTP API и MCP-сервер поверх barkdocs-engine. Своего состояния не хранит.

mod mcp;

use std::time::Duration;

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct AppState {
    http: reqwest::Client,
    auth_url: String,
    engine_url: String,
}

#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Admin,
    User,
}

/// Кто обращается: роль ключа и, для ключей агентов, владелец и сам ключ — для учёта запросов.
#[derive(Clone, Copy, Deserialize)]
pub struct Identity {
    role: Role,
    user_id: Option<i64>,
    key_id: Option<i64>,
}

/// Через что пришёл запрос; движок ведёт по нему раздельную статистику.
#[derive(Clone, Copy)]
pub enum Channel {
    Api,
    Mcp,
}

impl Channel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Mcp => "mcp",
        }
    }
}

/// Ошибка auth-сервиса закрывает доступ: без проверки ключа запросы не проходят.
async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let Some(authorization) = request.headers().get(header::AUTHORIZATION).cloned() else {
        return unauthorized();
    };
    let verified = state
        .http
        .post(format!("{}/verify", state.auth_url))
        .header(header::AUTHORIZATION, authorization)
        .send()
        .await;
    let identity = match verified {
        Ok(response) if response.status() == StatusCode::UNAUTHORIZED => return unauthorized(),
        Ok(response) => match response.error_for_status() {
            Ok(response) => response.json::<Identity>().await,
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    };
    match identity {
        Ok(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(err) => {
            tracing::error!("barkdocs-auth недоступен: {err}");
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "сервис авторизации недоступен",
            )
        }
    }
}

async fn require_admin(
    Extension(identity): Extension<Identity>,
    request: Request,
    next: Next,
) -> Response {
    if identity.role != Role::Admin {
        return error_response(StatusCode::FORBIDDEN, "нужен admin-ключ");
    }
    next.run(request).await
}

fn unauthorized() -> Response {
    let mut response = error_response(StatusCode::UNAUTHORIZED, "нужен действительный API-ключ");
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Передаёт клиенту ответ движка как есть.
async fn forward(result: reqwest::Result<reqwest::Response>) -> Response {
    let response = match result {
        Ok(response) => response,
        Err(err) => {
            tracing::error!("barkdocs-engine недоступен: {err}");
            return error_response(StatusCode::BAD_GATEWAY, "движок недоступен");
        }
    };
    let status = response.status();
    match response.bytes().await {
        Ok(body) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err(err) => {
            tracing::error!("barkdocs-engine: {err}");
            error_response(StatusCode::BAD_GATEWAY, "движок недоступен")
        }
    }
}

#[derive(Deserialize, Serialize)]
struct ResolveParams {
    name: String,
}

#[derive(Deserialize, Serialize)]
struct DocsParams {
    library_id: String,
    query: String,
    limit: Option<i64>,
}

impl AppState {
    /// Запрос к движку от имени клиента: заголовки сообщают, кому засчитать запрос.
    pub fn engine_get<Q: Serialize>(
        &self,
        identity: &Identity,
        channel: Channel,
        path: &str,
        query: &Q,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .get(format!("{}{path}", self.engine_url))
            .query(query)
            .header("x-barkdocs-channel", channel.as_str());
        if let Some(user_id) = identity.user_id {
            request = request.header("x-barkdocs-user-id", user_id);
        }
        if let Some(key_id) = identity.key_id {
            request = request.header("x-barkdocs-key-id", key_id);
        }
        request
    }

    fn engine_post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.engine_url))
            .json(body)
    }
}

async fn resolve(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Query(params): Query<ResolveParams>,
) -> Response {
    let request = state.engine_get(&identity, Channel::Api, "/libraries/resolve", &params);
    forward(request.send().await).await
}

async fn docs(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Query(params): Query<DocsParams>,
) -> Response {
    let request = state.engine_get(&identity, Channel::Api, "/docs", &params);
    forward(request.send().await).await
}

async fn add_library(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    forward(state.engine_post("/libraries", &body).send().await).await
}

async fn sync_library(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    forward(state.engine_post("/sync", &body).send().await).await
}

fn router(state: AppState) -> Router {
    let admin = Router::new()
        .route("/v1/libraries", post(add_library))
        .route("/v1/libraries/sync", post(sync_library))
        .route_layer(middleware::from_fn(require_admin));
    let protected = Router::new()
        .route("/v1/libraries/resolve", get(resolve))
        .route("/v1/docs", get(docs))
        .route("/mcp", post(mcp::handle))
        .merge(admin)
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let state = AppState {
        // Поиск в реестрах при resolve идёт синхронно и может занять десятки секунд.
        http: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(180))
            .build()?,
        auth_url: std::env::var("AUTH_URL")?,
        engine_url: std::env::var("ENGINE_URL")?,
    };
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("barkdocs-web слушает {addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
