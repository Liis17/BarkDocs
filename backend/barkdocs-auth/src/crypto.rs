//! Токены и пароли. Токены хранятся в БД только как sha256, пароли — как argon2id.

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use sha2::{Digest, Sha256};

/// Префикс API-ключа агента.
pub const KEY_PREFIX: &str = "bd_";
/// Префикс сессии веб-консоли: по нему сессию не спутать с ключом агента.
pub const SESSION_PREFIX: &str = "bds_";

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn generate_token(prefix: &str) -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(format!("{prefix}{}", hex::encode(bytes)))
}

/// Сравнение без раннего выхода, чтобы время ответа не выдавало совпавший префикс.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Хеширование занимает десятки миллисекунд CPU, поэтому выполняется вне async-потоков.
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|e| anyhow::anyhow!("argon2: {e}"))
    })
    .await?
}

/// Хеш, с которым сверяется пароль несуществующего пользователя: ответ занимает столько же
/// времени, и по нему нельзя узнать, зарегистрирован ли логин.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Ik9mzE0s0VQ8wFzbwfaDOXwZmDjrBQ0bUUt9n1KUwgs";

pub async fn verify_password(password: String, hash: Option<String>) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || {
        let exists = hash.is_some();
        let stored = hash.unwrap_or_else(|| DUMMY_HASH.to_string());
        let parsed = PasswordHash::new(&stored).map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(exists && ok)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_prefixed() {
        let a = generate_token(KEY_PREFIX).unwrap();
        let b = generate_token(KEY_PREFIX).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("bd_"));
        assert_eq!(a.len(), 3 + 64);
        assert!(generate_token(SESSION_PREFIX).unwrap().starts_with("bds_"));
    }

    #[test]
    fn constant_time_eq_compares_whole_input() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[tokio::test]
    async fn password_roundtrip() {
        let hash = hash_password("correct horse".into()).await.unwrap();
        assert!(
            verify_password("correct horse".into(), Some(hash.clone()))
                .await
                .unwrap()
        );
        assert!(!verify_password("wrong".into(), Some(hash)).await.unwrap());
    }

    #[tokio::test]
    async fn dummy_hash_is_valid_but_never_matches() {
        assert!(PasswordHash::new(DUMMY_HASH).is_ok());
        assert!(!verify_password("anything".into(), None).await.unwrap());
    }
}
