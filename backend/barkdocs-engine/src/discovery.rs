//! Поиск библиотеки по имени в реестрах и регистрация её источников.

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::{
    AppState, db,
    http::{SafeClient, check_url},
    sources::{github, llms},
    sync,
};

/// Сколько кандидатов из разных реестров регистрируем на одно имя.
const MAX_CANDIDATES: usize = 3;

/// Что известно о библиотеке до обращения к GitHub.
#[derive(Debug, Default, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub description: String,
    pub repo: Option<(String, String)>,
    pub site: Option<Url>,
}

/// Защищает от мусора в URL реестров и бессмысленных запросов на свободный текст.
pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '/'))
}

fn registry_url(base: &str, segments: &[&str]) -> Option<Url> {
    let mut url = Url::parse(base).ok()?;
    url.path_segments_mut().ok()?.extend(segments);
    Some(url)
}

fn candidate(
    name: &str,
    description: Option<&str>,
    repo: Option<&str>,
    urls: &[Option<&str>],
) -> Candidate {
    let mut result = Candidate {
        name: name.to_string(),
        description: description.unwrap_or_default().to_string(),
        repo: repo.and_then(github::parse_repo),
        site: None,
    };
    // Домашняя страница часто указывает на тот же GitHub.
    for raw in urls.iter().flatten() {
        if let Some(parsed) = github::parse_repo(raw) {
            result.repo.get_or_insert(parsed);
        } else if result.site.is_none()
            && let Ok(url) = Url::parse(raw)
            && check_url(&url).is_ok()
        {
            result.site = Some(url);
        }
    }
    result
}

fn from_npm(name: &str, json: &Value) -> Candidate {
    let repository = &json["repository"];
    let repo = repository.as_str().or_else(|| repository["url"].as_str());
    candidate(
        name,
        json["description"].as_str(),
        repo,
        &[json["homepage"].as_str()],
    )
}

fn from_crates(name: &str, json: &Value) -> Candidate {
    let info = &json["crate"];
    let documentation = info["documentation"].as_str();
    let homepage = info["homepage"].as_str();
    // docs.rs — автогенерация API, поэтому собственный сайт проекта полезнее.
    let on_docs_rs = documentation.is_some_and(|d| d.contains("://docs.rs/"));
    let sites = if on_docs_rs {
        [homepage, documentation]
    } else {
        [documentation, homepage]
    };
    candidate(
        name,
        info["description"].as_str(),
        info["repository"].as_str(),
        &sites,
    )
}

fn from_pypi(name: &str, json: &Value) -> Candidate {
    let info = &json["info"];
    let links: Vec<(String, &str)> = info["project_urls"]
        .as_object()
        .map(|urls| {
            urls.iter()
                .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    let find = |keys: &[&str]| {
        links
            .iter()
            .find(|(label, _)| keys.iter().any(|k| label.contains(k)))
            .map(|(_, url)| *url)
    };
    candidate(
        name,
        info["summary"].as_str(),
        find(&["source", "repository", "github", "code"]),
        &[
            find(&["documentation", "docs"]),
            find(&["homepage"]),
            info["home_page"].as_str(),
        ],
    )
}

async fn lookup(http: &SafeClient, url: Option<Url>) -> Option<Value> {
    let url = url?;
    match http.json(&url).await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("реестр {url}: {err:#}");
            None
        }
    }
}

/// Кандидаты из npm, crates.io и PyPI. Недоступный реестр не мешает остальным.
pub async fn search_registries(http: &SafeClient, name: &str) -> Vec<Candidate> {
    let (npm, crates, pypi) = tokio::join!(
        lookup(
            http,
            registry_url("https://registry.npmjs.org", &[name, "latest"])
        ),
        lookup(
            http,
            registry_url("https://crates.io/api/v1/crates", &[name])
        ),
        lookup(http, registry_url("https://pypi.org/pypi", &[name, "json"])),
    );
    let candidates = [
        npm.map(|json| from_npm(name, &json)),
        crates.map(|json| from_crates(name, &json)),
        pypi.map(|json| from_pypi(name, &json)),
    ];
    let mut unique: Vec<Candidate> = Vec::new();
    for found in candidates.into_iter().flatten() {
        let same_repo = found.repo.is_some() && unique.iter().any(|c| c.repo == found.repo);
        if !same_repo && (found.repo.is_some() || found.site.is_some()) {
            unique.push(found);
        }
    }
    unique.truncate(MAX_CANDIDATES);
    unique
}

/// Кандидат по прямой ссылке: GitHub-репозиторий или сайт документации.
pub fn from_link(raw: &str) -> anyhow::Result<Candidate> {
    if let Some(repo) = github::parse_repo(raw) {
        return Ok(Candidate {
            name: repo.1.clone(),
            repo: Some(repo),
            ..Default::default()
        });
    }
    let url = Url::parse(raw)?;
    check_url(&url)?;
    Ok(Candidate {
        name: url.host_str().unwrap_or_default().to_string(),
        site: Some(url),
        ..Default::default()
    })
}

fn site_slug(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = url.path().trim_matches('/').to_ascii_lowercase();
    if path.is_empty() {
        format!("/{host}")
    } else {
        format!("/{host}/{path}")
    }
}

/// Создаёт или обновляет библиотеку, выбирает источники и запускает их первую синхронизацию.
/// У сайта llms-файл заменяет обход. Сайт индексируется и при наличии репозитория: у многих
/// проектов в репозитории только README, поэтому часть результатов может повторяться.
/// Возвращает `None`, если у кандидата не осталось рабочих источников.
pub async fn register(
    state: &AppState,
    candidate: Candidate,
    alias: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let mut library = db::NewLibrary {
        slug: String::new(),
        name: candidate.name,
        description: candidate.description,
        stars: 0,
        alias: alias.map(str::to_ascii_lowercase),
    };
    let mut site = candidate.site;
    let mut sources: Vec<(&str, String)> = Vec::new();

    if let Some((owner, repo)) = &candidate.repo {
        match github::repo_meta(state, owner, repo).await? {
            Some(meta) => {
                library.slug = format!("/{}", meta.full_name.to_ascii_lowercase());
                library.name = meta.name;
                library.stars = meta.stargazers_count;
                if let Some(description) = meta.description.filter(|d| !d.is_empty()) {
                    library.description = description;
                }
                if site.is_none() {
                    site = meta
                        .homepage
                        .and_then(|raw| Url::parse(&raw).ok())
                        .filter(|url| {
                            check_url(url).is_ok() && github::parse_repo(url.as_str()).is_none()
                        });
                }
                sources.push(("github", format!("https://github.com/{}", meta.full_name)));
            }
            None => tracing::warn!("репозиторий {owner}/{repo} не найден"),
        }
    }

    if let Some(site) = &site {
        if library.slug.is_empty() {
            library.slug = site_slug(site);
        }
        match llms::probe(&state.http, site).await {
            Some(llms_url) => sources.push(("llms", llms_url.to_string())),
            None => sources.push(("site", site.to_string())),
        }
    }
    if sources.is_empty() {
        return Ok(None);
    }

    let slug = library.slug.clone();
    let library_id = db::upsert_library(&state.pool, &library, &sources).await?;
    sync::start_pending(state, library_id).await?;
    Ok(Some(slug))
}

#[derive(Deserialize)]
pub struct AddLibrary {
    pub url: Option<String>,
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo(owner: &str, name: &str) -> Option<(String, String)> {
        Some((owner.to_string(), name.to_string()))
    }

    #[test]
    fn parses_npm_metadata() {
        let c = from_npm(
            "next",
            &json!({
                "description": "The framework",
                "homepage": "https://nextjs.org",
                "repository": {"type": "git", "url": "git+https://github.com/vercel/next.js.git"}
            }),
        );
        assert_eq!(c.repo, repo("vercel", "next.js"));
        assert_eq!(c.site.unwrap().as_str(), "https://nextjs.org/");
        assert_eq!(c.description, "The framework");
    }

    #[test]
    fn npm_repository_may_be_a_shorthand_string() {
        let c = from_npm("x", &json!({"repository": "github:o/r"}));
        assert_eq!(c.repo, repo("o", "r"));
    }

    #[test]
    fn crates_prefer_project_site_over_docs_rs() {
        let c = from_crates(
            "serde",
            &json!({"crate": {
                "description": "ser",
                "repository": "https://github.com/serde-rs/serde",
                "documentation": "https://docs.rs/serde",
                "homepage": "https://serde.rs"
            }}),
        );
        assert_eq!(c.repo, repo("serde-rs", "serde"));
        assert_eq!(c.site.unwrap().as_str(), "https://serde.rs/");

        let only_docs_rs = from_crates(
            "x",
            &json!({"crate": {"documentation": "https://docs.rs/x"}}),
        );
        assert_eq!(only_docs_rs.site.unwrap().as_str(), "https://docs.rs/x");

        let own_docs = from_crates(
            "y",
            &json!({"crate": {"documentation": "https://y.dev/docs", "homepage": "https://y.dev"}}),
        );
        assert_eq!(own_docs.site.unwrap().as_str(), "https://y.dev/docs");
    }

    #[test]
    fn parses_pypi_metadata() {
        let c = from_pypi(
            "requests",
            &json!({"info": {
                "summary": "HTTP",
                "home_page": null,
                "project_urls": {
                    "Source": "https://github.com/psf/requests",
                    "Documentation": "https://requests.readthedocs.io"
                }
            }}),
        );
        assert_eq!(c.repo, repo("psf", "requests"));
        assert_eq!(c.site.unwrap().as_str(), "https://requests.readthedocs.io/");
    }

    #[test]
    fn homepage_on_github_becomes_repo_not_site() {
        let c = from_npm("x", &json!({"homepage": "https://github.com/o/r#readme"}));
        assert_eq!(c.repo, repo("o", "r"));
        assert_eq!(c.site, None);
    }

    #[test]
    fn metadata_cannot_point_inside_the_network() {
        let c = from_npm(
            "x",
            &json!({"homepage": "http://169.254.169.254/latest/meta-data"}),
        );
        assert_eq!(c.site, None);
    }

    #[test]
    fn link_is_classified_and_validated() {
        assert_eq!(
            from_link("https://github.com/o/r").unwrap().repo,
            repo("o", "r")
        );
        assert!(from_link("https://x.dev/docs").unwrap().site.is_some());
        assert!(from_link("http://10.0.0.1/docs").is_err());
        assert!(from_link("file:///etc/passwd").is_err());
    }

    #[test]
    fn package_name_validation() {
        assert!(valid_package_name("@scope/pkg-1.2_x"));
        assert!(!valid_package_name(""));
        assert!(!valid_package_name("how to use react"));
        assert!(!valid_package_name(&"a".repeat(101)));
    }

    #[test]
    fn registry_url_encodes_scoped_names() {
        let url = registry_url("https://registry.npmjs.org", &["@scope/pkg", "latest"]).unwrap();
        assert_eq!(
            url.as_str(),
            "https://registry.npmjs.org/@scope%2Fpkg/latest"
        );
        let url = registry_url("https://pypi.org/pypi", &["../admin", "json"]).unwrap();
        assert_eq!(url.as_str(), "https://pypi.org/pypi/..%2Fadmin/json");
    }

    #[test]
    fn site_slug_uses_host_and_path() {
        assert_eq!(
            site_slug(&Url::parse("https://X.dev/Docs/").unwrap()),
            "/x.dev/docs"
        );
        assert_eq!(site_slug(&Url::parse("https://x.dev").unwrap()), "/x.dev");
    }
}
