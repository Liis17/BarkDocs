//! Сессия веб-консоли в cookie, flash-сообщения и защита форм от CSRF.

use axum::{
    extract::{FromRequestParts, Request},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};

use crate::{AppState, client::User, views};

pub const SESSION_COOKIE: &str = "bd_session";
const FLASH_COOKIE: &str = "bd_flash";
const SESSION_MAX_AGE: u32 = 30 * 24 * 60 * 60;

pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

/// Secure ставится, если консоль открыта по HTTPS напрямую или за обратным прокси.
fn secure(headers: &HeaderMap, state: &AppState) -> bool {
    state.cookie_secure
        || headers
            .get("x-forwarded-proto")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"https"))
}

fn set_cookie(
    headers: &HeaderMap,
    state: &AppState,
    name: &str,
    value: &str,
    max_age: u32,
) -> HeaderValue {
    let secure = if secure(headers, state) {
        "; Secure"
    } else {
        ""
    };
    let cookie =
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}");
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub fn session_cookie(headers: &HeaderMap, state: &AppState, token: &str) -> HeaderValue {
    set_cookie(headers, state, SESSION_COOKIE, token, SESSION_MAX_AGE)
}

pub fn clear_session_cookie(headers: &HeaderMap, state: &AppState) -> HeaderValue {
    set_cookie(headers, state, SESSION_COOKIE, "", 0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlashKind {
    Ok,
    Error,
}

/// Сообщение о результате действия, которое показывается один раз после редиректа.
#[derive(Clone, Debug, PartialEq)]
pub struct Flash {
    pub kind: FlashKind,
    pub message: String,
}

fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn decode_flash(raw: &str) -> Option<Flash> {
    let decoded: String = url::form_urlencoded::parse(format!("f={raw}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())?;
    let (kind, message) = decoded.split_once(':')?;
    let kind = match kind {
        "ok" => FlashKind::Ok,
        "error" => FlashKind::Error,
        _ => return None,
    };
    Some(Flash {
        kind,
        message: message.chars().take(500).collect(),
    })
}

/// Редирект с сообщением для следующей страницы.
pub fn redirect_with(
    headers: &HeaderMap,
    state: &AppState,
    to: &str,
    kind: FlashKind,
    message: &str,
) -> Response {
    let kind = match kind {
        FlashKind::Ok => "ok",
        FlashKind::Error => "error",
    };
    let value = encode(&format!("{kind}:{message}"));
    let mut response = Redirect::to(to).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(headers, state, FLASH_COOKIE, &value, 60),
    );
    response
}

/// Читает flash из cookie; страница, показавшая сообщение, обязана его стереть.
pub struct PendingFlash(pub Option<Flash>);

impl<S: Send + Sync> FromRequestParts<S> for PendingFlash {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            cookie(&parts.headers, FLASH_COOKIE).and_then(decode_flash),
        ))
    }
}

/// Стирает показанный flash, чтобы он не появился снова.
pub async fn consume_flash(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let had_flash = cookie(request.headers(), FLASH_COOKIE).is_some();
    let headers = request.headers().clone();
    let is_get = request.method() == Method::GET;
    let mut response = next.run(request).await;
    if had_flash && is_get && !response.status().is_redirection() {
        response.headers_mut().append(
            header::SET_COOKIE,
            set_cookie(&headers, &state, FLASH_COOKIE, "", 0),
        );
    }
    response
}

/// Вошедший пользователь. Без сессии — редирект на вход.
#[derive(Clone)]
pub struct CurrentUser {
    pub token: String,
    pub user: User,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let Some(token) = cookie(&parts.headers, SESSION_COOKIE).filter(|t| !t.is_empty()) else {
            return Err(Redirect::to("/login").into_response());
        };
        match state.services.me(token).await {
            Ok(user) => Ok(Self {
                token: token.to_string(),
                user,
            }),
            Err(err) if err.is_unauthorized() => {
                let mut response = Redirect::to("/login").into_response();
                response.headers_mut().append(
                    header::SET_COOKIE,
                    clear_session_cookie(&parts.headers, state),
                );
                Err(response)
            }
            Err(err) => Err(views::error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                &err.message,
            )),
        }
    }
}

/// Вошедший администратор; остальным — 403.
pub struct AdminUser(pub CurrentUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let current = CurrentUser::from_request_parts(parts, state).await?;
        if !current.user.is_admin() {
            return Err(views::error_page(
                StatusCode::FORBIDDEN,
                "Раздел доступен только администратору.",
            ));
        }
        Ok(Self(current))
    }
}

/// Формы принимаются только со своего сайта. Cookie и так SameSite=Strict, проверка
/// Origin и Sec-Fetch-Site закрывает старые браузеры и поддомены.
pub async fn csrf_guard(request: Request, next: Next) -> Response {
    if matches!(*request.method(), Method::GET | Method::HEAD) {
        return next.run(request).await;
    }
    if !same_origin(request.headers()) {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string()
        };
        tracing::warn!(
            "отклонена форма {}: origin={} host={} sec-fetch-site={}",
            request.uri().path(),
            header("origin"),
            header("host"),
            header("sec-fetch-site")
        );
        return views::error_page(
            StatusCode::FORBIDDEN,
            "Запрос отклонён: форма отправлена с другого сайта.",
        );
    }
    next.run(request).await
}

/// Sec-Fetch-Site выставляет сам браузер, и подделать его страница не может; без него
/// (старые браузеры) сверяется Origin. Origin бывает `null` у форм со строгой политикой
/// Referer, поэтому решает именно Sec-Fetch-Site.
fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return matches!(site, "same-origin" | "none");
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok());
    let origin_host = url::Url::parse(origin).ok().and_then(|u| {
        let host = u.host_str()?.to_string();
        Some(match u.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        })
    });
    matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

/// Путь возврата после действия: только локальный, чтобы форма не увела на чужой сайт.
pub fn local_path(raw: Option<&str>, fallback: &str) -> String {
    match raw {
        Some(path) if path.starts_with('/') && !path.starts_with("//") && !path.contains('\\') => {
            path.to_string()
        }
        _ => fallback.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn reads_cookie_among_others() {
        let h = headers(&[("cookie", "a=1; bd_session=bds_x"), ("cookie", "b=2")]);
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("bds_x"));
        assert_eq!(cookie(&h, "b"), Some("2"));
        assert_eq!(cookie(&h, "c"), None);
    }

    #[test]
    fn flash_roundtrip_through_cookie_encoding() {
        let raw = encode("error:репозиторий o/r не найден; да");
        assert_eq!(
            decode_flash(&raw),
            Some(Flash {
                kind: FlashKind::Error,
                message: "репозиторий o/r не найден; да".into()
            })
        );
        assert_eq!(decode_flash("evil:x"), None);
    }

    #[test]
    fn cross_site_forms_are_rejected() {
        assert!(same_origin(&headers(&[("host", "c.dev")])));
        assert!(same_origin(&headers(&[
            ("host", "c.dev"),
            ("origin", "https://c.dev")
        ])));
        assert!(same_origin(&headers(&[
            ("host", "localhost:8083"),
            ("origin", "http://localhost:8083")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "c.dev"),
            ("origin", "https://evil.dev")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "c.dev"),
            ("origin", "null")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "c.dev"),
            ("sec-fetch-site", "cross-site")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "c.dev"),
            ("sec-fetch-site", "same-site")
        ])));
    }

    #[test]
    fn return_paths_stay_local() {
        assert_eq!(local_path(Some("/users/3"), "/"), "/users/3");
        assert_eq!(local_path(Some("//evil.dev"), "/"), "/");
        assert_eq!(local_path(Some("https://evil.dev"), "/"), "/");
        assert_eq!(local_path(Some("/\\evil.dev"), "/"), "/");
        assert_eq!(local_path(None, "/keys"), "/keys");
    }
}
