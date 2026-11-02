//! Аккаунты веб-консоли: регистрация, вход, смена пароля и управление пользователями.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::{
    AppState,
    crypto::{SESSION_PREFIX, generate_token, hash_password, hash_token, verify_password},
    error::{ApiError, ApiResult},
    principal::{Principal, Role, USER_COLUMNS, User},
};

const SESSION_DAYS: i32 = 30;
const MIN_PASSWORD_CHARS: usize = 8;
const MAX_PASSWORD_CHARS: usize = 256;
const MAX_FAILED_LOGINS: u32 = 10;
const FAILED_LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Нормализует логин: регистр не важен, допустим узкий набор символов.
pub fn normalize_login(raw: &str) -> Result<String, ApiError> {
    let login = raw.trim().to_ascii_lowercase();
    let valid = (3..=64).contains(&login.len())
        && login
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    if valid {
        Ok(login)
    } else {
        Err(ApiError::invalid(
            "логин: от 3 до 64 символов, латиница, цифры и . _ - @",
        ))
    }
}

pub fn check_password(password: &str) -> Result<(), ApiError> {
    let chars = password.chars().count();
    if (MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&chars) {
        Ok(())
    } else {
        Err(ApiError::invalid(format!(
            "пароль: от {MIN_PASSWORD_CHARS} до {MAX_PASSWORD_CHARS} символов"
        )))
    }
}

/// Ограничивает подбор пароля к одному логину. Хранится в памяти: после перезапуска
/// счётчики обнуляются, но перебор всё равно упирается в стоимость argon2.
#[derive(Default)]
pub struct LoginLimiter(Mutex<HashMap<String, (u32, Instant)>>);

impl LoginLimiter {
    fn blocked(&self, login: &str) -> bool {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(login).is_some_and(|(count, since)| {
            *count >= MAX_FAILED_LOGINS && since.elapsed() < FAILED_LOGIN_WINDOW
        })
    }

    fn failed(&self, login: &str) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 10_000 {
            map.retain(|_, (_, since)| since.elapsed() < FAILED_LOGIN_WINDOW);
        }
        let entry = map.entry(login.to_string()).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= FAILED_LOGIN_WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    fn succeeded(&self, login: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(login);
    }
}

#[derive(Deserialize)]
pub struct Credentials {
    login: String,
    password: String,
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<Credentials>,
) -> ApiResult<(StatusCode, Json<User>)> {
    if !state.allow_registration {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "регистрация отключена администратором",
        ));
    }
    let login = normalize_login(&req.login)?;
    check_password(&req.password)?;
    if state.admin_login.as_deref() == Some(login.as_str()) {
        return Err(ApiError::new(StatusCode::CONFLICT, "логин уже занят"));
    }
    let hash = hash_password(req.password).await?;
    let sql = format!(
        "WITH u AS (INSERT INTO users (login, password_hash) VALUES ($1, $2)
                    ON CONFLICT (login) DO NOTHING RETURNING *)
         SELECT {USER_COLUMNS} FROM u"
    );
    let user: Option<User> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&login)
        .bind(hash)
        .fetch_optional(&state.pool)
        .await?;
    let user = user.ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "логин уже занят"))?;
    tracing::info!("зарегистрирован пользователь {}", user.login);
    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Serialize)]
pub struct Session {
    token: String,
    user: User,
}

#[derive(sqlx::FromRow)]
struct StoredUser {
    id: i64,
    password_hash: String,
    disabled: bool,
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<Credentials>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let wrong = || ApiError::new(StatusCode::UNAUTHORIZED, "неверный логин или пароль");
    let Ok(login) = normalize_login(&req.login) else {
        return Err(wrong());
    };
    if state.login_limiter.blocked(&login) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "слишком много неудачных попыток, попробуйте через 15 минут",
        ));
    }
    let stored: Option<StoredUser> = sqlx::query_as(
        "SELECT id, password_hash, (disabled_at IS NOT NULL) AS disabled
         FROM users WHERE login = $1",
    )
    .bind(&login)
    .fetch_optional(&state.pool)
    .await?;
    let hash = stored.as_ref().map(|u| u.password_hash.clone());
    if !verify_password(req.password, hash).await? {
        state.login_limiter.failed(&login);
        return Err(wrong());
    }
    let stored = stored.ok_or_else(wrong)?;
    if stored.disabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "аккаунт заблокирован администратором",
        ));
    }
    state.login_limiter.succeeded(&login);

    let token = generate_token(SESSION_PREFIX)?;
    sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
        .execute(&state.pool)
        .await?;
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at)
         VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(hash_token(&token))
    .bind(stored.id)
    .bind(SESSION_DAYS)
    .execute(&state.pool)
    .await?;
    let user = fetch_user(&state, stored.id)
        .await?
        .ok_or_else(|| ApiError::not_found("пользователь"))?;
    Ok((StatusCode::CREATED, Json(Session { token, user })))
}

pub async fn logout(State(state): State<AppState>, principal: Principal) -> ApiResult<StatusCode> {
    if let Principal::Session { token_hash, .. } = principal {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&state.pool)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(principal: Principal) -> ApiResult<Json<User>> {
    match principal {
        Principal::Session { user, .. } => Ok(Json(user)),
        Principal::Master => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "мастер-ключ не привязан к аккаунту",
        )),
    }
}

#[derive(Deserialize)]
pub struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// Меняет пароль и завершает остальные сессии пользователя.
pub async fn change_password(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<PasswordChange>,
) -> ApiResult<StatusCode> {
    let Principal::Session { user, token_hash } = principal else {
        return Err(ApiError::forbidden());
    };
    check_password(&req.new_password)?;
    let current: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&state.pool)
        .await?;
    if !verify_password(req.current_password, Some(current)).await? {
        return Err(ApiError::invalid("текущий пароль указан неверно"));
    }
    let hash = hash_password(req.new_password).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user.id)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
        .bind(user.id)
        .bind(token_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fetch_user(state: &AppState, id: i64) -> sqlx::Result<Option<User>> {
    let sql = format!("SELECT {USER_COLUMNS} FROM users u WHERE u.id = $1");
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&state.pool)
        .await
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UserSummary {
    #[sqlx(flatten)]
    #[serde(flatten)]
    user: User,
    active_keys: i64,
    last_used_at: Option<String>,
}

const SUMMARY_SQL: &str = "
    (SELECT count(*) FROM api_keys k WHERE k.user_id = u.id AND k.revoked_at IS NULL) AS active_keys,
    (SELECT max(k.last_used_at)::text FROM api_keys k WHERE k.user_id = u.id) AS last_used_at";

pub async fn list_users(
    State(state): State<AppState>,
    principal: Principal,
) -> ApiResult<Json<Vec<UserSummary>>> {
    principal.require_admin()?;
    let sql = format!("SELECT {USER_COLUMNS}, {SUMMARY_SQL} FROM users u ORDER BY u.id");
    let users = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(users))
}

pub async fn get_user(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<i64>,
) -> ApiResult<Json<UserSummary>> {
    principal.require_admin()?;
    let sql = format!("SELECT {USER_COLUMNS}, {SUMMARY_SQL} FROM users u WHERE u.id = $1");
    let user: Option<UserSummary> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?;
    user.map(Json)
        .ok_or_else(|| ApiError::not_found("пользователь"))
}

#[derive(Deserialize)]
pub struct UserUpdate {
    disabled: Option<bool>,
    role: Option<Role>,
}

/// Блокировка сразу закрывает доступ: сессии удаляются, ключи перестают проходить проверку.
pub async fn update_user(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<i64>,
    Json(req): Json<UserUpdate>,
) -> ApiResult<Json<UserSummary>> {
    principal.require_admin()?;
    if principal.user_id() == Some(id) {
        return Err(ApiError::invalid(
            "нельзя заблокировать себя или снять с себя роль администратора",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("пользователь"));
    }
    if let Some(role) = req.role {
        sqlx::query("UPDATE users SET role = $2 WHERE id = $1")
            .bind(id)
            .bind(role)
            .execute(&mut *tx)
            .await?;
    }
    match req.disabled {
        Some(true) => {
            sqlx::query(
                "UPDATE users SET disabled_at = coalesce(disabled_at, now()) WHERE id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM sessions WHERE user_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        Some(false) => {
            sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        None => {}
    }
    tx.commit().await?;
    get_user(State(state), principal, Path(id)).await
}

/// Создаёт администратора из ADMIN_LOGIN/ADMIN_PASSWORD или обновляет ему пароль и роль:
/// так забытый пароль восстанавливается сменой переменной окружения и перезапуском.
pub async fn bootstrap_admin(state: &AppState, password: &str) -> anyhow::Result<()> {
    let Some(login) = &state.admin_login else {
        return Ok(());
    };
    check_password(password).map_err(|e| anyhow::anyhow!("ADMIN_PASSWORD: {}", e.1))?;
    let hash = hash_password(password.to_string()).await?;
    sqlx::query(
        "INSERT INTO users (login, password_hash, role) VALUES ($1, $2, 'admin')
         ON CONFLICT (login) DO UPDATE SET password_hash = EXCLUDED.password_hash,
                                           role = 'admin', disabled_at = NULL",
    )
    .bind(login)
    .bind(hash)
    .execute(&state.pool)
    .await?;
    tracing::info!("администратор {login} готов");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_is_normalized_and_validated() {
        assert_eq!(normalize_login("  Alice.Dev ").unwrap(), "alice.dev");
        assert!(normalize_login("ab").is_err());
        assert!(normalize_login("имя").is_err());
        assert!(normalize_login("a b c").is_err());
        assert!(normalize_login(&"a".repeat(65)).is_err());
    }

    #[test]
    fn password_length_is_checked_in_chars() {
        assert!(check_password("1234567").is_err());
        assert!(check_password("пароль12").is_ok());
        assert!(check_password(&"x".repeat(257)).is_err());
    }

    #[test]
    fn limiter_blocks_after_repeated_failures() {
        let limiter = LoginLimiter::default();
        for _ in 0..MAX_FAILED_LOGINS {
            assert!(!limiter.blocked("bob"));
            limiter.failed("bob");
        }
        assert!(limiter.blocked("bob"));
        assert!(!limiter.blocked("alice"));
        limiter.succeeded("bob");
        assert!(!limiter.blocked("bob"));
    }
}
