pub mod account;
pub mod keys;
pub mod libraries;
pub mod overview;
pub mod users;

use axum::{http::StatusCode, response::Response};

use crate::{client::ServiceError, views};

/// Страница не собралась из-за ошибки внутреннего сервиса.
pub fn failed(err: ServiceError) -> Response {
    let status = match err.status {
        Some(StatusCode::NOT_FOUND) => StatusCode::NOT_FOUND,
        Some(StatusCode::FORBIDDEN) => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_GATEWAY,
    };
    views::error_page(status, &err.message)
}

/// Нормализует необязательное поле формы: пустая строка значит «не задано».
pub fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
