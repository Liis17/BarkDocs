//! Доки из GitHub-репозитория: README и markdown из каталогов документации.
//! Tarball читается потоком и не распаковывается на диск, код из него не исполняется.

use std::{io::Read, time::Duration};

use anyhow::{Context, bail};
use flate2::read::GzDecoder;
use futures_util::TryStreamExt;
use reqwest::{RequestBuilder, Response, StatusCode, header};
use serde::Deserialize;
use tokio_util::io::{StreamReader, SyncIoBridge};
use url::Url;

use super::{Doc, Fetched};
use crate::AppState;

const API: &str = "https://api.github.com";
const API_TIMEOUT: Duration = Duration::from_secs(30);
const TARBALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_COMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Защита от gzip-бомбы: столько байт максимум прочитаем после распаковки.
const MAX_UNPACKED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_DOC_BYTES: usize = 64 * 1024 * 1024;
const MAX_DOCS: usize = 5000;

const DOC_DIRS: [&str; 3] = ["docs", "doc", "documentation"];
const EXCLUDED_DIRS: [&str; 7] = [
    "node_modules",
    "vendor",
    ".github",
    "test",
    "tests",
    "__tests__",
    "fixtures",
];

#[derive(Deserialize)]
pub struct RepoMeta {
    pub full_name: String,
    pub name: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub stargazers_count: i64,
    pub default_branch: String,
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Понимает все формы ссылок из реестров: `git+https://github.com/o/r.git`,
/// `git@github.com:o/r`, `github:o/r`, `https://github.com/o/r/tree/main/docs`.
pub fn parse_repo(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("git+").unwrap_or(raw);
    let path = if let Some(rest) = raw.strip_prefix("github:") {
        rest
    } else if let Some(rest) = raw.strip_prefix("git@github.com:") {
        rest
    } else {
        let without_scheme = raw.split_once("://").map(|(_, rest)| rest)?;
        let without_user = without_scheme
            .strip_prefix("git@")
            .unwrap_or(without_scheme);
        let without_www = without_user.strip_prefix("www.").unwrap_or(without_user);
        without_www.strip_prefix("github.com/")?
    };
    let path = path.split(['?', '#']).next()?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (valid_segment(owner) && valid_segment(repo)).then(|| (owner.to_string(), repo.to_string()))
}

fn api_request(
    state: &AppState,
    path: &str,
    accept: &str,
    timeout: Duration,
) -> anyhow::Result<RequestBuilder> {
    let url = Url::parse(&format!("{API}{path}"))?;
    let mut request = state
        .http
        .get(&url)?
        .timeout(timeout)
        .header(header::ACCEPT, accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = &state.github_token {
        request = request.bearer_auth(token);
    }
    Ok(request)
}

/// `None` при 404; при исчерпании лимита ошибка объясняет, что делать.
async fn checked(request: RequestBuilder) -> anyhow::Result<Option<Response>> {
    let response = request.send().await?;
    let status = response.status();
    if status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0"));
    if rate_limited {
        bail!("исчерпан лимит GitHub API: задайте GITHUB_TOKEN или дождитесь сброса лимита");
    }
    if !status.is_success() {
        bail!("GitHub API: HTTP {status}");
    }
    Ok(Some(response))
}

pub async fn repo_meta(
    state: &AppState,
    owner: &str,
    repo: &str,
) -> anyhow::Result<Option<RepoMeta>> {
    let request = api_request(
        state,
        &format!("/repos/{owner}/{repo}"),
        "application/vnd.github+json",
        API_TIMEOUT,
    )?;
    match checked(request).await? {
        Some(response) => Ok(Some(response.json().await?)),
        None => Ok(None),
    }
}

pub async fn fetch(state: &AppState, url: &str, previous: Option<&str>) -> anyhow::Result<Fetched> {
    let (owner, repo) = parse_repo(url).with_context(|| format!("не GitHub-репозиторий: {url}"))?;
    let meta = repo_meta(state, &owner, &repo)
        .await?
        .with_context(|| format!("репозиторий {owner}/{repo} не найден"))?;

    let sha_request = api_request(
        state,
        &format!("/repos/{owner}/{repo}/commits/{}", meta.default_branch),
        "application/vnd.github.sha",
        API_TIMEOUT,
    )?;
    let sha = checked(sha_request)
        .await?
        .context("ветка по умолчанию не найдена")?
        .text()
        .await?;
    if previous == Some(sha.as_str()) {
        return Ok(Fetched::Unchanged);
    }

    let tarball_request = api_request(
        state,
        &format!("/repos/{owner}/{repo}/tarball/{sha}"),
        "application/vnd.github+json",
        TARBALL_TIMEOUT,
    )?;
    let response = checked(tarball_request)
        .await?
        .context("tarball не найден")?;
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    let reader = SyncIoBridge::new(StreamReader::new(stream));
    let files =
        tokio::task::spawn_blocking(move || extract_docs(reader.take(MAX_COMPRESSED_BYTES)))
            .await??;

    let base = format!("https://github.com/{owner}/{repo}/blob/{sha}");
    let docs = files
        .into_iter()
        .map(|(path, markdown)| Doc {
            url: format!("{base}/{path}"),
            markdown,
        })
        .collect();
    Ok(Fetched::Updated { version: sha, docs })
}

/// Читает tar.gz и возвращает пары (путь внутри репозитория, текст) подходящих файлов.
fn extract_docs<R: Read>(compressed: R) -> anyhow::Result<Vec<(String, String)>> {
    let mut archive = tar::Archive::new(GzDecoder::new(compressed).take(MAX_UNPACKED_BYTES));
    let mut docs = Vec::new();
    let mut total = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() || entry.size() > MAX_FILE_BYTES {
            continue;
        }
        let raw_path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let Some(path) = doc_path(&raw_path) else {
            continue;
        };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        total += bytes.len();
        if total > MAX_TOTAL_DOC_BYTES || docs.len() >= MAX_DOCS {
            tracing::warn!("достигнут лимит документов, остаток архива пропущен");
            break;
        }
        docs.push((path, String::from_utf8_lossy(&bytes).into_owned()));
    }
    Ok(docs)
}

/// Путь внутри репозитория, если файл является документацией. Первый сегмент в tarball
/// GitHub — служебный каталог `owner-repo-sha`. Небезопасные пути отбрасываются.
fn doc_path(raw: &str) -> Option<String> {
    if raw.starts_with('/') || raw.contains('\\') || raw.contains('\0') {
        return None;
    }
    let segments: Vec<&str> = raw.split('/').collect();
    if segments.iter().any(|s| matches!(*s, ".." | ".")) {
        return None;
    }
    let (_, inner) = segments.split_first()?;
    let (file, dirs) = inner.split_last()?;
    let lower = file.to_ascii_lowercase();
    if !(lower.ends_with(".md") || lower.ends_with(".mdx") || lower.ends_with(".markdown")) {
        return None;
    }
    let lower_dirs: Vec<String> = dirs.iter().map(|d| d.to_ascii_lowercase()).collect();
    if lower_dirs
        .iter()
        .any(|d| EXCLUDED_DIRS.contains(&d.as_str()))
    {
        return None;
    }
    let is_root_readme = dirs.is_empty() && lower.starts_with("readme");
    let in_doc_dir = lower_dirs.iter().any(|d| DOC_DIRS.contains(&d.as_str()));
    (is_root_readme || in_doc_dir).then(|| inner.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            // Имя пишем в заголовок напрямую: set_path отказался бы принять вредоносный путь.
            let name = &mut header.as_old_mut().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn parses_repository_url_forms() {
        let expected = Some(("o".to_string(), "r".to_string()));
        for raw in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "git+https://github.com/o/r.git",
            "git://github.com/o/r.git",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "github:o/r",
            "https://www.github.com/o/r/tree/main/docs",
            "https://github.com/o/r#readme",
        ] {
            assert_eq!(parse_repo(raw), expected, "{raw}");
        }
    }

    #[test]
    fn rejects_non_github_and_unsafe_repositories() {
        for raw in [
            "https://gitlab.com/o/r",
            "https://github.com/o",
            "https://github.com.evil.com/o/r",
            "https://github.com/../r",
            "https://github.com/o/r%2f..",
            "not a url",
        ] {
            assert_eq!(parse_repo(raw), None, "{raw}");
        }
    }

    #[test]
    fn selects_only_documentation_paths() {
        assert_eq!(doc_path("o-r-abc/README.md").as_deref(), Some("README.md"));
        assert_eq!(
            doc_path("o-r-abc/docs/a/b.mdx").as_deref(),
            Some("docs/a/b.mdx")
        );
        assert_eq!(
            doc_path("o-r-abc/packages/x/Docs/a.MD").as_deref(),
            Some("packages/x/Docs/a.MD")
        );
        for path in [
            "o-r-abc/src/lib.md",
            "o-r-abc/docs/image.png",
            "o-r-abc/node_modules/x/docs/a.md",
            "o-r-abc/docs/tests/a.md",
            "o-r-abc/sub/README.md",
            "README.md",
        ] {
            assert_eq!(doc_path(path), None, "{path}");
        }
    }

    #[test]
    fn rejects_path_traversal() {
        for path in [
            "../docs/a.md",
            "o-r-abc/../../docs/a.md",
            "o-r-abc/docs/../../../etc/a.md",
            "/etc/docs/a.md",
            "o-r-abc/docs/..\\a.md",
            "o-r-abc/./docs/a.md",
        ] {
            assert_eq!(doc_path(path), None, "{path}");
        }
    }

    #[test]
    fn extracts_docs_and_ignores_malicious_entries() {
        let archive = tarball(&[
            ("o-r-abc/README.md", b"# Readme"),
            ("o-r-abc/docs/guide.md", b"# Guide"),
            ("o-r-abc/src/main.rs", b"fn main() {}"),
            ("o-r-abc/../../evil/docs/pwn.md", b"# Pwned"),
        ]);
        let docs = extract_docs(archive.as_slice()).unwrap();
        assert_eq!(
            docs,
            vec![
                ("README.md".to_string(), "# Readme".to_string()),
                ("docs/guide.md".to_string(), "# Guide".to_string()),
            ]
        );
    }

    #[test]
    fn skips_oversized_files() {
        let big = vec![b'x'; MAX_FILE_BYTES as usize + 1];
        let archive = tarball(&[("o-r-abc/docs/big.md", &big), ("o-r-abc/docs/ok.md", b"ok")]);
        let docs = extract_docs(archive.as_slice()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].0, "docs/ok.md");
    }

    #[test]
    fn corrupted_archive_is_an_error() {
        assert!(extract_docs(&b"not a gzip"[..]).is_err());
    }
}
