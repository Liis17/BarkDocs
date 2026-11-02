//! Клиенты внутренних сервисов. Консоль своего состояния не хранит: аккаунты и ключи
//! живут в barkdocs-auth, библиотеки и журнал запросов — в barkdocs-engine.

use reqwest::{RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Debug)]
pub struct ServiceError {
    pub status: Option<StatusCode>,
    pub message: String,
}

impl ServiceError {
    pub fn is_unauthorized(&self) -> bool {
        self.status == Some(StatusCode::UNAUTHORIZED)
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ServiceResult<T> = Result<T, ServiceError>;

async fn send(service: &str, request: RequestBuilder) -> ServiceResult<reqwest::Response> {
    let response = request.send().await.map_err(|err| {
        tracing::error!("{service} недоступен: {err}");
        ServiceError {
            status: None,
            message: format!("{service} недоступен, попробуйте позже"),
        }
    })?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body: Value = response.json().await.unwrap_or_default();
    let message = body["error"]
        .as_str()
        .map_or_else(|| format!("{service}: ошибка {status}"), str::to_string);
    Err(ServiceError {
        status: Some(status),
        message,
    })
}

async fn json<T: DeserializeOwned>(service: &str, request: RequestBuilder) -> ServiceResult<T> {
    send(service, request).await?.json().await.map_err(|err| {
        tracing::error!("{service}: некорректный ответ: {err}");
        ServiceError {
            status: None,
            message: format!("{service}: некорректный ответ"),
        }
    })
}

async fn empty(service: &str, request: RequestBuilder) -> ServiceResult<()> {
    send(service, request).await.map(|_| ())
}

#[derive(Clone)]
pub struct Services {
    pub http: reqwest::Client,
    pub auth_url: String,
    pub engine_url: String,
}

const AUTH: &str = "сервис авторизации";
const ENGINE: &str = "движок документации";

// --- barkdocs-auth -------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub id: i64,
    pub login: String,
    pub role: String,
    pub created_at: String,
    pub disabled: bool,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UserSummary {
    #[serde(flatten)]
    pub user: User,
    pub active_keys: i64,
    pub last_used_at: Option<String>,
}

#[derive(Deserialize)]
pub struct Session {
    pub token: String,
    pub user: User,
}

#[derive(Clone, Debug, Deserialize)]
pub struct KeyInfo {
    pub id: i64,
    pub name: String,
    pub user_id: Option<i64>,
    pub login: Option<String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatedKey {
    pub name: String,
    pub key: String,
}

/// Чьи ключи запросить.
pub enum KeyScope {
    Own,
    User(i64),
    All,
}

impl Services {
    fn auth(&self, method: reqwest::Method, path: &str, token: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.auth_url))
            .bearer_auth(token)
    }

    pub async fn login(&self, login: &str, password: &str) -> ServiceResult<Session> {
        let request = self
            .http
            .post(format!("{}/sessions", self.auth_url))
            .json(&json!({ "login": login, "password": password }));
        json(AUTH, request).await
    }

    pub async fn register(&self, login: &str, password: &str) -> ServiceResult<User> {
        let request = self
            .http
            .post(format!("{}/register", self.auth_url))
            .json(&json!({ "login": login, "password": password }));
        json(AUTH, request).await
    }

    pub async fn logout(&self, token: &str) -> ServiceResult<()> {
        empty(AUTH, self.auth(reqwest::Method::DELETE, "/sessions", token)).await
    }

    pub async fn me(&self, token: &str) -> ServiceResult<User> {
        json(AUTH, self.auth(reqwest::Method::GET, "/me", token)).await
    }

    pub async fn change_password(
        &self,
        token: &str,
        current: &str,
        new: &str,
    ) -> ServiceResult<()> {
        let request = self
            .auth(reqwest::Method::POST, "/me/password", token)
            .json(&json!({ "current_password": current, "new_password": new }));
        empty(AUTH, request).await
    }

    pub async fn keys(&self, token: &str, scope: KeyScope) -> ServiceResult<Vec<KeyInfo>> {
        let request = self.auth(reqwest::Method::GET, "/keys", token);
        let request = match scope {
            KeyScope::Own => request,
            KeyScope::User(id) => request.query(&[("user_id", id)]),
            KeyScope::All => request.query(&[("all", "true")]),
        };
        json(AUTH, request).await
    }

    pub async fn create_key(&self, token: &str, name: &str) -> ServiceResult<CreatedKey> {
        let request = self
            .auth(reqwest::Method::POST, "/keys", token)
            .json(&json!({ "name": name }));
        json(AUTH, request).await
    }

    pub async fn revoke_key(&self, token: &str, id: i64) -> ServiceResult<()> {
        empty(
            AUTH,
            self.auth(reqwest::Method::DELETE, &format!("/keys/{id}"), token),
        )
        .await
    }

    pub async fn users(&self, token: &str) -> ServiceResult<Vec<UserSummary>> {
        json(AUTH, self.auth(reqwest::Method::GET, "/users", token)).await
    }

    pub async fn user(&self, token: &str, id: i64) -> ServiceResult<UserSummary> {
        json(
            AUTH,
            self.auth(reqwest::Method::GET, &format!("/users/{id}"), token),
        )
        .await
    }

    pub async fn update_user(&self, token: &str, id: i64, change: Value) -> ServiceResult<()> {
        let request = self
            .auth(reqwest::Method::PATCH, &format!("/users/{id}"), token)
            .json(&change);
        empty(AUTH, request).await
    }
}

// --- barkdocs-engine -----------------------------------------------------------------------
// У движка нет своей авторизации: права проверяет консоль до обращения к нему.

#[derive(Clone, Debug, Deserialize)]
pub struct Library {
    pub library_id: String,
    pub name: String,
    pub description: String,
    pub stars: i64,
    pub chunks: i64,
    pub status: String,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LibrarySummary {
    #[serde(flatten)]
    pub library: Library,
    pub sources: i64,
    pub requests: i64,
    pub last_requested_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Source {
    pub id: i64,
    pub kind: String,
    pub url: String,
    pub version: Option<String>,
    pub last_checked_at: Option<String>,
    pub last_success_at: Option<String>,
    pub next_check_at: String,
    pub last_error: Option<String>,
    pub syncing: bool,
    pub chunks: i64,
}

#[derive(Debug, Deserialize)]
pub struct LibraryDetail {
    pub library: Library,
    pub aliases: Vec<String>,
    pub created_at: String,
    pub sources: Vec<Source>,
}

#[derive(Debug, Deserialize)]
pub struct Hit {
    pub url: String,
    pub heading: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct Docs {
    pub results: Vec<Hit>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Totals {
    pub requests: i64,
    pub docs: i64,
    pub resolves: i64,
    pub chars: i64,
    pub results: i64,
    pub today: i64,
    pub in_period: i64,
    pub users: i64,
    pub keys: i64,
    pub libraries: i64,
    pub last_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Day {
    pub day: String,
    pub requests: i64,
    pub docs: i64,
}

#[derive(Debug, Deserialize)]
pub struct LibraryUsage {
    pub library: String,
    pub requests: i64,
    pub chars: i64,
    pub users: i64,
    pub last_at: String,
}

#[derive(Debug, Deserialize)]
pub struct KeyUsage {
    pub key_id: i64,
    pub user_id: Option<i64>,
    pub requests: i64,
    pub docs: i64,
    pub chars: i64,
    pub last_at: String,
}

#[derive(Debug, Deserialize)]
pub struct UserUsage {
    pub user_id: i64,
    pub requests: i64,
    pub docs: i64,
    pub chars: i64,
    pub last_at: String,
}

#[derive(Debug, Deserialize)]
pub struct TopQuery {
    pub library: Option<String>,
    pub query: String,
    pub count: i64,
}

#[derive(Debug, Deserialize)]
pub struct TopPage {
    pub url: String,
    pub count: i64,
}

#[derive(Debug, Deserialize)]
pub struct ChannelUsage {
    pub channel: String,
    pub requests: i64,
}

#[derive(Debug, Deserialize)]
pub struct RecentRequest {
    pub at: String,
    pub user_id: Option<i64>,
    pub key_id: Option<i64>,
    pub channel: String,
    pub kind: String,
    pub library: Option<String>,
    pub query: String,
    pub results: i32,
    pub chars: i64,
}

#[derive(Debug, Deserialize)]
pub struct Stats {
    pub days: i32,
    pub totals: Totals,
    pub daily: Vec<Day>,
    pub libraries: Vec<LibraryUsage>,
    pub keys: Vec<KeyUsage>,
    pub users: Vec<UserUsage>,
    pub queries: Vec<TopQuery>,
    pub pages: Vec<TopPage>,
    pub channels: Vec<ChannelUsage>,
    pub recent: Vec<RecentRequest>,
}

#[derive(Default, Serialize)]
pub struct StatsFilter {
    pub user_id: Option<i64>,
    pub key_id: Option<i64>,
    pub library_id: Option<String>,
    pub days: i32,
}

#[derive(Serialize)]
pub struct NewSource {
    pub kind: Option<String>,
    pub url: String,
}

#[derive(Serialize)]
pub struct CustomLibrary {
    pub slug: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub aliases: Vec<String>,
    pub sources: Vec<NewSource>,
}

impl Services {
    fn engine(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.engine_url))
    }

    pub async fn stats(&self, filter: &StatsFilter) -> ServiceResult<Stats> {
        json(
            ENGINE,
            self.engine(reqwest::Method::GET, "/stats").query(filter),
        )
        .await
    }

    pub async fn libraries(&self, q: Option<&str>) -> ServiceResult<Vec<LibrarySummary>> {
        let request = self.engine(reqwest::Method::GET, "/libraries");
        let request = match q {
            Some(q) => request.query(&[("q", q)]),
            None => request,
        };
        json(ENGINE, request).await
    }

    pub async fn library(&self, id: &str) -> ServiceResult<LibraryDetail> {
        let request = self
            .engine(reqwest::Method::GET, "/libraries/detail")
            .query(&[("library_id", id)]);
        json(ENGINE, request).await
    }

    /// Автоматическое добавление: по ссылке или по имени пакета с поиском в реестрах.
    pub async fn add_library(&self, body: Value) -> ServiceResult<Vec<Library>> {
        json(
            ENGINE,
            self.engine(reqwest::Method::POST, "/libraries").json(&body),
        )
        .await
    }

    pub async fn create_library(&self, library: &CustomLibrary) -> ServiceResult<Library> {
        let request = self
            .engine(reqwest::Method::POST, "/libraries/custom")
            .json(library);
        json(ENGINE, request).await
    }

    pub async fn update_library(&self, body: Value) -> ServiceResult<()> {
        empty(
            ENGINE,
            self.engine(reqwest::Method::PATCH, "/libraries")
                .json(&body),
        )
        .await
    }

    pub async fn delete_library(&self, id: &str) -> ServiceResult<()> {
        let request = self
            .engine(reqwest::Method::DELETE, "/libraries")
            .query(&[("library_id", id)]);
        empty(ENGINE, request).await
    }

    pub async fn sync_library(&self, id: &str) -> ServiceResult<()> {
        let request = self
            .engine(reqwest::Method::POST, "/sync")
            .json(&json!({ "library_id": id }));
        empty(ENGINE, request).await
    }

    pub async fn add_source(
        &self,
        library_id: &str,
        kind: Option<&str>,
        url: &str,
    ) -> ServiceResult<()> {
        let request = self
            .engine(reqwest::Method::POST, "/sources")
            .json(&json!({ "library_id": library_id, "kind": kind, "url": url }));
        empty(ENGINE, request).await
    }

    pub async fn delete_source(&self, id: i64) -> ServiceResult<()> {
        empty(
            ENGINE,
            self.engine(reqwest::Method::DELETE, &format!("/sources/{id}")),
        )
        .await
    }

    pub async fn sync_source(&self, id: i64) -> ServiceResult<()> {
        empty(
            ENGINE,
            self.engine(reqwest::Method::POST, &format!("/sources/{id}/sync")),
        )
        .await
    }

    /// Пробный поиск из консоли засчитывается администратору по каналу web.
    pub async fn search_docs(
        &self,
        user_id: i64,
        library_id: &str,
        query: &str,
    ) -> ServiceResult<Docs> {
        let request = self
            .engine(reqwest::Method::GET, "/docs")
            .header("x-barkdocs-channel", "web")
            .header("x-barkdocs-user-id", user_id)
            .query(&[("library_id", library_id), ("query", query), ("limit", "5")]);
        json(ENGINE, request).await
    }
}
