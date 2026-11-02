//! API-ключи агентов: выдача, отзыв и проверка. Ключ показывается один раз при создании.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};

use crate::{
    AppState,
    crypto::{KEY_PREFIX, constant_time_eq, generate_token, hash_token},
    error::{ApiError, ApiResult},
    principal::{Principal, Role, bearer},
};

const MAX_KEY_NAME_CHARS: usize = 100;
/// Защита от засорения БД, на объём запросов не влияет.
const MAX_ACTIVE_KEYS_PER_USER: i64 = 100;

#[derive(Serialize)]
pub struct VerifyResponse {
    role: Role,
    /// Владелец и ключ нужны шлюзу для учёта запросов; у мастер-ключа их нет.
    user_id: Option<i64>,
    key_id: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct ActiveKey {
    id: i64,
    user_id: Option<i64>,
}

/// Ключи агентов всегда получают роль user, даже если их владелец администратор:
/// утечка ключа агента не должна давать права на изменение индекса.
pub async fn verify(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<VerifyResponse>> {
    let key = bearer(&headers).ok_or_else(ApiError::unauthorized)?;
    let hash = hash_token(key);
    if constant_time_eq(&hash, &state.admin_key_hash) {
        return Ok(Json(VerifyResponse {
            role: Role::Admin,
            user_id: None,
            key_id: None,
        }));
    }
    if !key.starts_with(KEY_PREFIX) {
        return Err(ApiError::unauthorized());
    }
    let active: Option<ActiveKey> = sqlx::query_as(
        "SELECT k.id, k.user_id FROM api_keys k LEFT JOIN users u ON u.id = k.user_id
         WHERE k.key_hash = $1 AND k.revoked_at IS NULL
           AND (k.user_id IS NULL OR u.disabled_at IS NULL)",
    )
    .bind(hash)
    .fetch_optional(&state.pool)
    .await?;
    let key = active.ok_or_else(ApiError::unauthorized)?;
    // Отметка об использовании не должна задерживать ответ; пишется не чаще раза в минуту.
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let updated = sqlx::query(
            "UPDATE api_keys SET last_used_at = now()
             WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
        )
        .bind(key.id)
        .execute(&pool)
        .await;
        if let Err(err) = updated {
            tracing::warn!("last_used_at ключа {}: {err}", key.id);
        }
    });
    Ok(Json(VerifyResponse {
        role: Role::User,
        user_id: key.user_id,
        key_id: Some(key.id),
    }))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct KeyInfo {
    id: i64,
    name: String,
    user_id: Option<i64>,
    login: Option<String>,
    created_at: String,
    revoked_at: Option<String>,
    last_used_at: Option<String>,
}

const KEY_COLUMNS: &str = "k.id, k.name, k.user_id, u.login, k.created_at::text AS created_at,
                           k.revoked_at::text AS revoked_at, k.last_used_at::text AS last_used_at";

#[derive(Deserialize)]
pub struct KeyScope {
    user_id: Option<i64>,
    #[serde(default)]
    all: bool,
}

/// Какие ключи видит обращающийся: `None` — все, `Some(id)` — ключи одного пользователя.
fn visible_owner(principal: &Principal, scope: &KeyScope) -> ApiResult<Option<i64>> {
    let own = principal.user_id();
    if scope.all || (scope.user_id.is_some() && scope.user_id != own) {
        principal.require_admin()?;
    }
    if scope.all {
        return Ok(None);
    }
    Ok(scope.user_id.or(own))
}

pub async fn list_keys(
    State(state): State<AppState>,
    principal: Principal,
    Query(scope): Query<KeyScope>,
) -> ApiResult<Json<Vec<KeyInfo>>> {
    let owner = visible_owner(&principal, &scope)?;
    let sql = format!(
        "SELECT {KEY_COLUMNS} FROM api_keys k LEFT JOIN users u ON u.id = k.user_id
         WHERE $1::bigint IS NULL OR k.user_id = $1
         ORDER BY k.revoked_at IS NOT NULL, k.id DESC"
    );
    let keys = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(owner)
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(keys))
}

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    name: String,
    /// Только для администратора: выдать ключ другому пользователю.
    user_id: Option<i64>,
}

#[derive(Serialize)]
pub struct CreatedKey {
    id: i64,
    name: String,
    user_id: Option<i64>,
    key: String,
}

pub async fn create_key(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<CreateKeyRequest>,
) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    let name = req.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_KEY_NAME_CHARS {
        return Err(ApiError::invalid(format!(
            "название ключа: от 1 до {MAX_KEY_NAME_CHARS} символов"
        )));
    }
    let owner = match req.user_id {
        Some(id) if principal.user_id() != Some(id) => {
            principal.require_admin()?;
            Some(id)
        }
        Some(id) => Some(id),
        None => principal.user_id(),
    };
    if let Some(owner) = owner {
        let active: Option<i64> = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM api_keys WHERE user_id = u.id AND revoked_at IS NULL)
             FROM users u WHERE u.id = $1",
        )
        .bind(owner)
        .fetch_optional(&state.pool)
        .await?;
        match active {
            None => return Err(ApiError::not_found("пользователь")),
            Some(n) if n >= MAX_ACTIVE_KEYS_PER_USER => {
                return Err(ApiError::invalid(format!(
                    "не больше {MAX_ACTIVE_KEYS_PER_USER} активных ключей, отзовите ненужные"
                )));
            }
            Some(_) => {}
        }
    }
    let key = generate_token(KEY_PREFIX)?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (name, key_hash, user_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&name)
    .bind(hash_token(&key))
    .bind(owner)
    .fetch_one(&state.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedKey {
            id,
            name,
            user_id: owner,
            key,
        }),
    ))
}

/// Пользователь отзывает только свои ключи, администратор — любые.
pub async fn revoke_key(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let owner = if principal.is_admin() {
        None
    } else {
        principal.user_id()
    };
    let result = sqlx::query(
        "UPDATE api_keys SET revoked_at = now()
         WHERE id = $1 AND revoked_at IS NULL AND ($2::bigint IS NULL OR user_id = $2)",
    )
    .bind(id)
    .bind(owner)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("активный ключ"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::User;

    fn session(id: i64, role: Role) -> Principal {
        Principal::Session {
            user: User {
                id,
                login: "u".into(),
                role,
                created_at: String::new(),
                disabled: false,
            },
            token_hash: Vec::new(),
        }
    }

    fn scope(user_id: Option<i64>, all: bool) -> KeyScope {
        KeyScope { user_id, all }
    }

    #[test]
    fn users_see_only_their_keys() {
        let user = session(7, Role::User);
        assert_eq!(
            visible_owner(&user, &scope(None, false)).ok(),
            Some(Some(7))
        );
        assert_eq!(
            visible_owner(&user, &scope(Some(7), false)).ok(),
            Some(Some(7))
        );
        assert!(visible_owner(&user, &scope(Some(8), false)).is_err());
        assert!(visible_owner(&user, &scope(None, true)).is_err());
    }

    #[test]
    fn admins_choose_scope() {
        let admin = session(1, Role::Admin);
        assert_eq!(
            visible_owner(&admin, &scope(None, false)).ok(),
            Some(Some(1))
        );
        assert_eq!(
            visible_owner(&admin, &scope(Some(8), false)).ok(),
            Some(Some(8))
        );
        assert_eq!(visible_owner(&admin, &scope(None, true)).ok(), Some(None));
        assert_eq!(
            visible_owner(&Principal::Master, &scope(None, false)).ok(),
            Some(None)
        );
    }
}
