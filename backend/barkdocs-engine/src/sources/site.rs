//! Обход HTML-документации: BFS в пределах хоста и префикса пути, с учётом robots.txt.

use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use anyhow::bail;
use scraper::{Html, Selector};
use texting_robots::Robot;
use url::Url;

use super::Doc;
use crate::http::{SafeClient, USER_AGENT};

const MAX_PAGES: usize = 300;
const MIN_DELAY: Duration = Duration::from_millis(200);
const MAX_DELAY: Duration = Duration::from_secs(5);
/// Короче этого страница считается пустой, например оболочка SPA.
const MIN_PAGE_CHARS: usize = 50;
const SKIPPED_TAGS: [&str; 9] = [
    "script", "style", "noscript", "nav", "header", "footer", "aside", "svg", "iframe",
];
const SKIPPED_EXTENSIONS: [&str; 16] = [
    ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".pdf", ".zip", ".gz", ".css", ".js",
    ".json", ".xml", ".woff", ".woff2",
];

/// Текст основной части страницы в markdown и все ссылки с неё.
pub fn extract_page(html: &str, base: &Url) -> (String, Vec<Url>) {
    let document = Html::parse_document(html);
    let content_selector = Selector::parse("main, article, [role=main]").expect("валидный CSS");
    let body_selector = Selector::parse("body").expect("валидный CSS");
    let link_selector = Selector::parse("a[href]").expect("валидный CSS");

    let content = document
        .select(&content_selector)
        .next()
        .or_else(|| document.select(&body_selector).next())
        .map(|el| el.html())
        .unwrap_or_default();
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED_TAGS.to_vec())
        .build();
    let markdown = converter.convert(&content).unwrap_or_default();

    let links = document
        .select(&link_selector)
        .filter_map(|a| base.join(a.value().attr("href")?).ok())
        .collect();
    (markdown, links)
}

/// Хост и префикс пути определяются по первой странице, уже после редиректов.
struct Scope {
    host: String,
    prefix: String,
}

impl Scope {
    fn of(start: &Url) -> Self {
        Self {
            host: start.host_str().unwrap_or_default().to_string(),
            prefix: start.path().trim_end_matches('/').to_string(),
        }
    }

    fn contains(&self, url: &Url) -> bool {
        let path = url.path().to_ascii_lowercase();
        matches!(url.scheme(), "http" | "https")
            && url.host_str() == Some(self.host.as_str())
            && url.path().starts_with(&self.prefix)
            && !SKIPPED_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
    }
}

/// Убирает части URL, не влияющие на содержимое, чтобы не обходить страницу дважды.
fn normalize(mut url: Url) -> Url {
    url.set_fragment(None);
    url.set_query(None);
    url
}

fn dedup_key(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_string()
}

async fn load_robots(http: &SafeClient, start: &Url) -> anyhow::Result<Option<Robot>> {
    let page = http.page(&start.join("/robots.txt")?).await?;
    match page {
        Some(page) if !page.is_html => Ok(Some(Robot::new(USER_AGENT, page.body.as_bytes())?)),
        _ => Ok(None),
    }
}

pub async fn fetch(http: &SafeClient, raw_url: &str) -> anyhow::Result<Vec<Doc>> {
    let start = normalize(Url::parse(raw_url)?);
    let robots = load_robots(http, &start).await?;
    let delay = robots
        .as_ref()
        .and_then(|r| r.delay)
        .map_or(MIN_DELAY, |seconds| {
            Duration::from_secs_f32(seconds.clamp(0.0, MAX_DELAY.as_secs_f32())).max(MIN_DELAY)
        });

    let mut queue = VecDeque::from([start.clone()]);
    let mut seen = HashSet::from([dedup_key(&start)]);
    let mut scope: Option<Scope> = None;
    let mut docs = Vec::new();
    let mut attempted = 0;
    let mut failed = 0;

    while let Some(url) = queue.pop_front() {
        if attempted >= MAX_PAGES {
            tracing::warn!("{start}: достигнут лимит {MAX_PAGES} страниц");
            break;
        }
        if robots.as_ref().is_some_and(|r| !r.allowed(url.as_str())) {
            continue;
        }
        if attempted > 0 {
            tokio::time::sleep(delay).await;
        }
        attempted += 1;

        let page = match http.page(&url).await {
            Ok(Some(page)) => page,
            Ok(None) => continue,
            Err(err) => {
                if docs.is_empty() && scope.is_none() {
                    return Err(err);
                }
                tracing::warn!("{url}: {err:#}");
                failed += 1;
                continue;
            }
        };
        let scope = scope.get_or_insert_with(|| Scope::of(&page.url));
        // Редирект мог увести за пределы документации.
        if !page.is_html || !scope.contains(&page.url) {
            continue;
        }

        let (markdown, links) = extract_page(&page.body, &page.url);
        for link in links {
            let link = normalize(link);
            if scope.contains(&link) && seen.insert(dedup_key(&link)) {
                queue.push_back(link);
            }
        }
        if markdown.chars().count() >= MIN_PAGE_CHARS {
            docs.push(Doc {
                url: page.url.to_string(),
                markdown,
            });
        }
    }

    // Иначе временный сбой сайта заменил бы индекс обрезанной версией.
    if failed * 2 > attempted {
        bail!("{start}: не загрузилось {failed} из {attempted} страниц");
    }
    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn extracts_main_content_as_markdown_and_links() {
        let html = r#"<html><body>
            <nav><a href="/docs/nav">Menu</a></nav>
            <main><h1>Title</h1><p>Hello <a href="./next">next</a></p>
              <pre><code>let x = 1;</code></pre>
              <script>alert(1)</script></main>
            <footer>Footer text</footer></body></html>"#;
        let (markdown, links) = extract_page(html, &url("https://x.dev/docs/page"));
        assert!(markdown.contains("# Title"), "{markdown}");
        assert!(markdown.contains("let x = 1;"), "{markdown}");
        assert!(!markdown.contains("alert"), "{markdown}");
        assert!(!markdown.contains("Footer text"), "{markdown}");
        assert!(!markdown.contains("Menu"), "{markdown}");
        assert!(links.contains(&url("https://x.dev/docs/nav")));
        assert!(links.contains(&url("https://x.dev/docs/next")));
    }

    #[test]
    fn falls_back_to_body_without_main() {
        let (markdown, _) = extract_page("<body><p>Plain page</p></body>", &url("https://x.dev/"));
        assert!(markdown.contains("Plain page"));
    }

    #[test]
    fn scope_limits_host_prefix_and_extensions() {
        let scope = Scope::of(&url("https://x.dev/docs/"));
        assert!(scope.contains(&url("https://x.dev/docs/intro")));
        assert!(scope.contains(&url("https://x.dev/docs")));
        assert!(!scope.contains(&url("https://x.dev/blog/post")));
        assert!(!scope.contains(&url("https://evil.dev/docs/intro")));
        assert!(!scope.contains(&url("https://x.dev/docs/logo.PNG")));
        assert!(!scope.contains(&url("mailto:a@x.dev")));
    }

    #[test]
    fn normalization_merges_equivalent_urls() {
        let a = normalize(url("https://x.dev/docs/?v=1#top"));
        let b = normalize(url("https://x.dev/docs"));
        assert_eq!(dedup_key(&a), dedup_key(&b));
    }

    #[test]
    fn robots_rules_apply_to_our_agent() {
        let robot = Robot::new(
            USER_AGENT,
            b"User-agent: *\nDisallow: /private\nCrawl-delay: 2",
        )
        .unwrap();
        assert!(robot.allowed("https://x.dev/docs"));
        assert!(!robot.allowed("https://x.dev/private/a"));
        assert_eq!(robot.delay, Some(2.0));
    }
}
