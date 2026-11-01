//! HTTP-клиент для недоверенных URL. Адреса приходят из метаданных пакетов и llms.txt,
//! поэтому запросы во внутреннюю сеть блокируются на трёх уровнях: проверка URL,
//! фильтрация результатов DNS и проверка каждого редиректа.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use reqwest::{
    StatusCode,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect,
};
use url::{Host, Url};

pub const USER_AGENT: &str = "BarkDocsBot/0.1";
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Лимит тела обычного ответа: страница, llms-full.txt, ответ реестра.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b)))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4() {
        return is_public_ipv4(v4);
    }
    let seg = ip.segments();
    // Только глобальный unicast (2000::/3), кроме документации и 6to4, встраивающего IPv4.
    (seg[0] & 0xe000) == 0x2000 && !(seg[0] == 0x2001 && seg[1] == 0x0db8) && seg[0] != 0x2002
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

/// Разрешены только http(s) на стандартных портах, без логина в URL и без IP внутренней сети.
pub fn check_url(url: &Url) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("запрещённая схема: {}", url.scheme());
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URL с учётными данными запрещён");
    }
    if url.port().is_some() {
        bail!("нестандартный порт запрещён");
    }
    let public = match url.host() {
        Some(Host::Domain(_)) => true,
        Some(Host::Ipv4(ip)) => is_public_ipv4(ip),
        Some(Host::Ipv6(ip)) => is_public_ipv6(ip),
        None => false,
    };
    if !public {
        bail!("адрес недоступен для запросов");
    }
    Ok(())
}

struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|addr| is_public_ip(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{} не имеет публичных адресов", name.as_str()).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Загруженная страница: итоговый URL после редиректов, признак HTML и текст.
pub struct Page {
    pub url: Url,
    pub is_html: bool,
    pub body: String,
}

#[derive(Clone)]
pub struct SafeClient(reqwest::Client);

impl SafeClient {
    pub fn new() -> anyhow::Result<Self> {
        let policy = redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("слишком много редиректов");
            }
            match check_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(err) => attempt.error(err.to_string()),
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // Прокси обошёл бы фильтр DNS: адрес резолвил бы уже он.
            .no_proxy()
            .dns_resolver(Arc::new(PublicResolver))
            .redirect(policy)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self(client))
    }

    /// GET с проверкой URL; таймаут всего запроса задаёт вызывающий код.
    pub fn get(&self, url: &Url) -> anyhow::Result<reqwest::RequestBuilder> {
        check_url(url)?;
        Ok(self.0.get(url.clone()))
    }

    /// Загружает страницу: `None` при 404/410, ошибка при прочих неуспешных статусах.
    pub async fn page(&self, url: &Url) -> anyhow::Result<Option<Page>> {
        let response = self.get(url)?.timeout(PAGE_TIMEOUT).send().await?;
        let status = response.status();
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("{url}: HTTP {status}");
        }
        Ok(Some(read_page(response).await?))
    }

    pub async fn json<T: serde::de::DeserializeOwned>(
        &self,
        url: &Url,
    ) -> anyhow::Result<Option<T>> {
        let Some(page) = self.page(url).await? else {
            return Ok(None);
        };
        let value = serde_json::from_str(&page.body).with_context(|| format!("{url}: не JSON"))?;
        Ok(Some(value))
    }
}

/// Читает тело ответа с лимитом размера.
pub async fn read_page(mut response: reqwest::Response) -> anyhow::Result<Page> {
    let url = response.url().clone();
    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("html"));
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            bail!("{url}: ответ больше {MAX_BODY_BYTES} байт");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Page {
        url,
        is_html,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> Url {
        Url::parse(url).unwrap()
    }

    #[test]
    fn blocks_internal_addresses() {
        for url in [
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[2002:7f00:1::]/",
        ] {
            assert!(
                check_url(&parse(url)).is_err(),
                "{url} должен быть запрещён"
            );
        }
    }

    #[test]
    fn blocks_other_schemes_ports_and_credentials() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "https://example.com:8443/",
            "https://user:pass@example.com/",
        ] {
            assert!(
                check_url(&parse(url)).is_err(),
                "{url} должен быть запрещён"
            );
        }
    }

    #[test]
    fn allows_public_urls() {
        for url in [
            "https://example.com/docs",
            "http://8.8.8.8/",
            "https://[2606:4700::1111]/",
        ] {
            assert!(check_url(&parse(url)).is_ok(), "{url} должен быть разрешён");
        }
    }

    #[tokio::test]
    async fn resolver_rejects_names_pointing_inside() {
        let name: Name = "localhost".parse().unwrap();
        assert!(PublicResolver.resolve(name).await.is_err());
    }
}
