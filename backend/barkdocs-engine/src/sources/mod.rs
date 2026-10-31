//! Источники документации. Каждый возвращает набор markdown-документов и их версию;
//! совпадение версии с прошлой означает, что переиндексация не нужна.

pub mod github;
pub mod llms;
pub mod site;

use sha2::{Digest, Sha256};

use crate::AppState;

pub struct Doc {
    pub url: String,
    pub markdown: String,
}

pub enum Fetched {
    Unchanged,
    Updated { version: String, docs: Vec<Doc> },
}

pub async fn fetch(
    state: &AppState,
    kind: &str,
    url: &str,
    previous: Option<&str>,
) -> anyhow::Result<Fetched> {
    let docs = match kind {
        "github" => return github::fetch(state, url, previous).await,
        "llms" => llms::fetch(&state.http, url).await?,
        "site" => site::fetch(&state.http, url).await?,
        other => anyhow::bail!("неизвестный тип источника: {other}"),
    };
    let version = content_version(&docs);
    if previous == Some(version.as_str()) {
        return Ok(Fetched::Unchanged);
    }
    Ok(Fetched::Updated { version, docs })
}

/// Хеш содержимого: сайты и llms.txt не дают надёжного признака изменений страниц.
fn content_version(docs: &[Doc]) -> String {
    let mut sorted: Vec<&Doc> = docs.iter().collect();
    sorted.sort_by(|a, b| a.url.cmp(&b.url));
    let mut hasher = Sha256::new();
    for doc in sorted {
        hasher.update(doc.url.as_bytes());
        hasher.update([0]);
        hasher.update(doc.markdown.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(url: &str, markdown: &str) -> Doc {
        Doc {
            url: url.into(),
            markdown: markdown.into(),
        }
    }

    #[test]
    fn version_ignores_order_but_tracks_content() {
        let a = content_version(&[doc("a", "1"), doc("b", "2")]);
        let b = content_version(&[doc("b", "2"), doc("a", "1")]);
        let c = content_version(&[doc("a", "1"), doc("b", "3")]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
