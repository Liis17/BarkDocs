//! Доки из llms-full.txt или llms.txt (https://llmstxt.org).

use futures_util::{StreamExt, stream};
use url::Url;

use super::{Doc, site};
use crate::http::{Page, SafeClient};

const MAX_LINKED_PAGES: usize = 300;
const CONCURRENT_PAGES: usize = 4;

/// SPA часто отдают 200 и index.html на любой путь, такой ответ не считается llms-файлом.
fn is_llms_body(page: &Page) -> bool {
    !page.is_html && !page.body.trim_start().starts_with('<') && !page.body.trim().is_empty()
}

/// Где искать llms-файл: сначала в каталоге документации, затем в корне сайта.
/// Полная версия приоритетнее индекса.
fn probe_urls(site: &Url) -> Vec<Url> {
    let dir = site.path().trim_end_matches('/');
    let mut urls = Vec::new();
    for prefix in [dir, ""] {
        for name in ["llms-full.txt", "llms.txt"] {
            if let Ok(url) = site.join(&format!("{prefix}/{name}"))
                && !urls.contains(&url)
            {
                urls.push(url);
            }
        }
    }
    urls
}

pub async fn probe(http: &SafeClient, site: &Url) -> Option<Url> {
    for candidate in probe_urls(site) {
        if let Ok(Some(page)) = http.page(&candidate).await
            && is_llms_body(&page)
        {
            return Some(candidate);
        }
    }
    None
}

/// Ссылки из markdown-индекса `[title](url)`, абсолютные и относительные.
fn parse_links(body: &str, base: &Url) -> Vec<Url> {
    let mut links = Vec::new();
    for part in body.split("](").skip(1) {
        let Some((target, _)) = part.split_once(')') else {
            continue;
        };
        let target = target.split_whitespace().next().unwrap_or_default();
        if let Ok(mut url) = base.join(target)
            && matches!(url.scheme(), "http" | "https")
        {
            url.set_fragment(None);
            if !links.contains(&url) {
                links.push(url);
            }
        }
    }
    links
}

pub async fn fetch(http: &SafeClient, raw_url: &str) -> anyhow::Result<Vec<Doc>> {
    let url = Url::parse(raw_url)?;
    let index = http
        .page(&url)
        .await?
        .filter(is_llms_body)
        .ok_or_else(|| anyhow::anyhow!("{url}: llms-файл не найден"))?;

    let links = if url.path().ends_with("llms-full.txt") {
        Vec::new()
    } else {
        parse_links(&index.body, &url)
    };
    let requested = links.len().min(MAX_LINKED_PAGES);

    let mut docs = vec![Doc {
        url: url.to_string(),
        markdown: index.body,
    }];
    let pages: Vec<_> = stream::iter(links.into_iter().take(MAX_LINKED_PAGES))
        .map(|link| async move { (link.clone(), http.page(&link).await) })
        .buffered(CONCURRENT_PAGES)
        .collect()
        .await;

    let mut failed = 0;
    for (link, result) in pages {
        match result {
            Ok(Some(page)) => {
                let markdown = if page.is_html {
                    site::extract_page(&page.body, &page.url).0
                } else {
                    page.body
                };
                docs.push(Doc {
                    url: link.to_string(),
                    markdown,
                });
            }
            Ok(None) => failed += 1,
            Err(err) => {
                tracing::warn!("{link}: {err:#}");
                failed += 1;
            }
        }
    }
    // Иначе временный сбой сайта заменил бы индекс обрезанной версией.
    if failed * 2 > requested {
        anyhow::bail!("{url}: не загрузилось {failed} из {requested} страниц");
    }
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_docs_directory_before_site_root() {
        let urls: Vec<String> = probe_urls(&Url::parse("https://x.dev/uv/").unwrap())
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            urls,
            [
                "https://x.dev/uv/llms-full.txt",
                "https://x.dev/uv/llms.txt",
                "https://x.dev/llms-full.txt",
                "https://x.dev/llms.txt",
            ]
        );
        assert_eq!(probe_urls(&Url::parse("https://x.dev").unwrap()).len(), 2);
    }

    #[test]
    fn parses_links_from_index() {
        let base = Url::parse("https://x.dev/llms.txt").unwrap();
        let body = "# X\n- [Intro](https://x.dev/intro.md): start\n- [Rel](/rel.md)\n\
                    - [Dup](https://x.dev/intro.md#top)\n- [Titled](https://y.dev/a.md \"t\")\n\
                    - [Mail](mailto:a@b.c)\n";
        let links: Vec<String> = parse_links(body, &base)
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            links,
            [
                "https://x.dev/intro.md",
                "https://x.dev/rel.md",
                "https://y.dev/a.md"
            ]
        );
    }

    #[test]
    fn html_response_is_not_an_llms_file() {
        let url = Url::parse("https://x.dev/llms.txt").unwrap();
        let page = |is_html, body: &str| Page {
            url: url.clone(),
            is_html,
            body: body.into(),
        };
        assert!(is_llms_body(&page(false, "# Docs")));
        assert!(!is_llms_body(&page(true, "# Docs")));
        assert!(!is_llms_body(&page(false, "<!doctype html>")));
        assert!(!is_llms_body(&page(false, "  ")));
    }
}
