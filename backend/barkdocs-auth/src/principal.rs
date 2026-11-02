//! Кто обращается к API управления: мастер-ключ из ADMIN_KEY или сессия веб-консоли.
//! Ключи агентов сюда не допускаются: они дают только доступ к документации.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, header, request::Parts},
};
use serde::{Deserialize, Serialize};

use crate::{
    AppState,
    crypto::{SESSION_PREFIX, constant_time_eq, hash_token},
    error::ApiError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub login: String,
    pub role: Role,
    pub created_at: String,
    pub disabled: bool,
}

pub const USER_COLUMNS: &str = "u.id, u.login, u.role, u.created_at::text AS created_at, \
                                (u.disabled_at IS NOT NULL) AS disabled";

pub enum Principal {
    /// Мастер-ключ ADMIN_KEY: полный доступ без аккаунта, для скриптов и аварийного входа.
    Master,
    Session {
        user: User,
        token_hash: Vec<u8>,
    },
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        match self {
            Self::Master => true,
            Self::Session { user, .. } => user.role == Role::Admin,
        }
    }

    pub fn user_id(&self) -> Option<i64> {
        match self {
            Self::Master => None,
            Self::Session { user, .. } => Some(user.id),
        }
    }

    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = bearer(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        let hash = hash_token(token);
        if constant_time_eq(&hash, &state.admin_key_hash) {
            return Ok(Self::Master);
        }
        if !token.starts_with(SESSION_PREFIX) {
            return Err(ApiError::unauthorized());
        }
        let sql = format!(
            "SELECT {USER_COLUMNS} FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now() AND u.disabled_at IS NULL"
        );
        let user: Option<User> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&hash)
            .fetch_optional(&state.pool)
            .await?;
        match user {
            Some(user) => Ok(Self::Session {
                user,
                token_hash: hash,
            }),
            None => Err(ApiError::unauthorized()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
