//! Учёт запросов к документации и статистика для веб-консоли.
//! Кто спрашивает, сообщает шлюз заголовками: движок доступен только из внутренней сети.

use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{AppState, api::ApiError, db::SearchHit};

pub const USER_HEADER: &str = "x-barkdocs-user-id";
pub const KEY_HEADER: &str = "x-barkdocs-key-id";
pub const CHANNEL_HEADER: &str = "x-barkdocs-channel";

const MAX_DAYS: i32 = 365;
/// Сколько дней показывает график, если период не ограничен.
const ALL_TIME_CHART_DAYS: i32 = 90;
const TOP_LIMIT: i64 = 20;
const RECENT_LIMIT: i64 = 50;

/// Кто сделал запрос. У мастер-ключа и внутренних вызовов владельца нет.
#[derive(Clone, Debug, PartialEq)]
pub struct Caller {
    pub user_id: Option<i64>,
    pub key_id: Option<i64>,
    pub channel: &'static str,
}

impl Caller {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let id = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
        };
        let channel = match headers.get(CHANNEL_HEADER).and_then(|v| v.to_str().ok()) {
            Some("mcp") => "mcp",
            Some("web") => "web",
            _ => "api",
        };
        Self {
            user_id: id(USER_HEADER),
            key_id: id(KEY_HEADER),
            channel,
        }
    }
}

pub struct Record {
    pub kind: &'static str,
    pub library: Option<String>,
    pub query: String,
    pub results: i32,
    pub chars: i64,
    pub urls: Vec<String>,
}

impl Record {
    pub fn resolve(name: &str, found: usize) -> Self {
        Self {
            kind: "resolve",
            library: None,
            query: name.to_string(),
            results: found as i32,
            chars: 0,
            urls: Vec::new(),
        }
    }

    /// Объём ответа считается в символах заголовков и текста: из него консоль оценивает токены.
    pub fn docs(library_id: &str, query: &str, hits: &[SearchHit]) -> Self {
        let mut urls: Vec<String> = Vec::new();
        for hit in hits {
            if !urls.contains(&hit.url) {
                urls.push(hit.url.clone());
            }
        }
        Self {
            kind: "docs",
            library: Some(library_id.to_string()),
            query: query.trim().to_string(),
            results: hits.len() as i32,
            chars: hits
                .iter()
                .map(|hit| (hit.heading.chars().count() + hit.content.chars().count()) as i64)
                .sum(),
            urls,
        }
    }
}

/// Запись идёт в фоне: сбой учёта не должен ломать ответ агенту.
pub fn record(pool: &PgPool, caller: Caller, record: Record) {
    let pool = pool.clone();
    tokio::spawn(async move {
        let inserted = sqlx::query(
            "INSERT INTO requests (user_id, key_id, channel, kind, library, query, results, chars, urls)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(caller.user_id)
        .bind(caller.key_id)
        .bind(caller.channel)
        .bind(record.kind)
        .bind(&record.library)
        .bind(&record.query)
        .bind(record.results)
        .bind(record.chars)
        .bind(&record.urls)
        .execute(&pool)
        .await;
        if let Err(err) = inserted {
            tracing::warn!("учёт запроса: {err}");
        }
    });
}

#[derive(Deserialize)]
pub struct StatsFilter {
    user_id: Option<i64>,
    key_id: Option<i64>,
    library_id: Option<String>,
    /// Период разбивок в днях; 0 — за всё время.
    days: Option<i32>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Totals {
    requests: i64,
    docs: i64,
    resolves: i64,
    chars: i64,
    results: i64,
    today: i64,
    in_period: i64,
    users: i64,
    keys: i64,
    libraries: i64,
    last_at: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Day {
    day: String,
    requests: i64,
    docs: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LibraryUsage {
    library: String,
    requests: i64,
    chars: i64,
    users: i64,
    last_at: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct KeyUsage {
    key_id: i64,
    user_id: Option<i64>,
    requests: i64,
    docs: i64,
    chars: i64,
    last_at: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UserUsage {
    user_id: i64,
    requests: i64,
    docs: i64,
    chars: i64,
    last_at: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TopQuery {
    library: Option<String>,
    query: String,
    count: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TopPage {
    url: String,
    count: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ChannelUsage {
    channel: String,
    requests: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct RecentRequest {
    at: String,
    user_id: Option<i64>,
    key_id: Option<i64>,
    channel: String,
    kind: String,
    library: Option<String>,
    query: String,
    results: i32,
    chars: i64,
}

#[derive(Serialize)]
pub struct Stats {
    days: i32,
    totals: Totals,
    daily: Vec<Day>,
    libraries: Vec<LibraryUsage>,
    keys: Vec<KeyUsage>,
    users: Vec<UserUsage>,
    queries: Vec<TopQuery>,
    pages: Vec<TopPage>,
    channels: Vec<ChannelUsage>,
    recent: Vec<RecentRequest>,
}

/// Фильтр по пользователю, ключу и библиотеке; `$4` — период в днях.
const FILTER: &str = "($1::bigint IS NULL OR r.user_id = $1)
    AND ($2::bigint IS NULL OR r.key_id = $2)
    AND ($3::text IS NULL OR r.library = $3)";
const PERIOD: &str = "($4::int = 0 OR r.at >= now() - make_interval(days => $4))";
const LAST_AT: &str = "to_char(max(r.at) AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI')";

#[derive(Clone)]
struct Bound {
    user_id: Option<i64>,
    key_id: Option<i64>,
    library: Option<String>,
    days: i32,
}

impl Bound {
    async fn fetch<T>(&self, pool: &PgPool, sql: String) -> sqlx::Result<Vec<T>>
    where
        T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
    {
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.user_id)
            .bind(self.key_id)
            .bind(&self.library)
            .bind(self.days)
            .fetch_all(pool)
            .await
    }
}

pub async fn stats(
    State(state): State<AppState>,
    Query(filter): Query<StatsFilter>,
) -> Result<Json<Stats>, ApiError> {
    let days = filter.days.unwrap_or(30).clamp(0, MAX_DAYS);
    let bound = Bound {
        user_id: filter.user_id,
        key_id: filter.key_id,
        library: filter.library_id.filter(|l| !l.is_empty()),
        days,
    };
    let chart = Bound {
        days: if days == 0 { ALL_TIME_CHART_DAYS } else { days },
        ..bound.clone()
    };
    let pool = &state.pool;

    let totals_sql = format!(
        "SELECT count(*) AS requests,
                count(*) FILTER (WHERE r.kind = 'docs') AS docs,
                count(*) FILTER (WHERE r.kind = 'resolve') AS resolves,
                coalesce(sum(r.chars), 0)::bigint AS chars,
                coalesce(sum(r.results), 0)::bigint AS results,
                count(*) FILTER (WHERE r.at >= date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') AS today,
                count(*) FILTER (WHERE {PERIOD}) AS in_period,
                count(DISTINCT r.user_id) AS users,
                count(DISTINCT r.key_id) AS keys,
                count(DISTINCT r.library) AS libraries,
                {LAST_AT} AS last_at
         FROM requests r WHERE {FILTER}"
    );
    // Дни без запросов тоже попадают в ряд, иначе график сжимает паузы.
    let daily_sql = format!(
        "SELECT to_char(d.day, 'YYYY-MM-DD') AS day,
                count(r.id) AS requests,
                count(r.id) FILTER (WHERE r.kind = 'docs') AS docs
         FROM generate_series(
                  (now() AT TIME ZONE 'UTC')::date - ($4::int - 1),
                  (now() AT TIME ZONE 'UTC')::date,
                  interval '1 day') AS d(day)
         LEFT JOIN requests r
                ON r.at >= d.day AT TIME ZONE 'UTC'
               AND r.at < (d.day + interval '1 day') AT TIME ZONE 'UTC'
               AND {FILTER}
         GROUP BY d.day ORDER BY d.day"
    );
    let libraries_sql = format!(
        "SELECT r.library, count(*) AS requests, coalesce(sum(r.chars), 0)::bigint AS chars,
                count(DISTINCT r.user_id) AS users, {LAST_AT} AS last_at
         FROM requests r
         WHERE r.kind = 'docs' AND r.library IS NOT NULL AND {FILTER} AND {PERIOD}
         GROUP BY r.library ORDER BY requests DESC, r.library LIMIT 100"
    );
    let keys_sql = format!(
        "SELECT r.key_id, max(r.user_id) AS user_id, count(*) AS requests,
                count(*) FILTER (WHERE r.kind = 'docs') AS docs,
                coalesce(sum(r.chars), 0)::bigint AS chars, {LAST_AT} AS last_at
         FROM requests r
         WHERE r.key_id IS NOT NULL AND {FILTER} AND {PERIOD}
         GROUP BY r.key_id ORDER BY requests DESC"
    );
    let users_sql = format!(
        "SELECT r.user_id, count(*) AS requests,
                count(*) FILTER (WHERE r.kind = 'docs') AS docs,
                coalesce(sum(r.chars), 0)::bigint AS chars, {LAST_AT} AS last_at
         FROM requests r
         WHERE r.user_id IS NOT NULL AND {FILTER} AND {PERIOD}
         GROUP BY r.user_id ORDER BY requests DESC"
    );
    let queries_sql = format!(
        "SELECT r.library, lower(r.query) AS query, count(*) AS count
         FROM requests r WHERE r.kind = 'docs' AND {FILTER} AND {PERIOD}
         GROUP BY r.library, lower(r.query) ORDER BY count DESC, query LIMIT {TOP_LIMIT}"
    );
    let pages_sql = format!(
        "SELECT p.url, count(*) AS count
         FROM requests r CROSS JOIN LATERAL unnest(r.urls) AS p(url)
         WHERE r.kind = 'docs' AND {FILTER} AND {PERIOD}
         GROUP BY p.url ORDER BY count DESC, p.url LIMIT {TOP_LIMIT}"
    );
    let channels_sql = format!(
        "SELECT r.channel, count(*) AS requests FROM requests r
         WHERE {FILTER} AND {PERIOD} GROUP BY r.channel ORDER BY requests DESC"
    );
    let recent_sql = format!(
        "SELECT to_char(r.at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS') AS at, r.user_id, r.key_id,
                r.channel, r.kind, r.library, r.query, r.results, r.chars
         FROM requests r WHERE {FILTER}
         ORDER BY r.at DESC, r.id DESC LIMIT {RECENT_LIMIT}"
    );

    let (totals, daily, libraries, keys, users, queries, pages, channels, recent) = tokio::try_join!(
        bound.fetch::<Totals>(pool, totals_sql),
        chart.fetch::<Day>(pool, daily_sql),
        bound.fetch::<LibraryUsage>(pool, libraries_sql),
        bound.fetch::<KeyUsage>(pool, keys_sql),
        bound.fetch::<UserUsage>(pool, users_sql),
        bound.fetch::<TopQuery>(pool, queries_sql),
        bound.fetch::<TopPage>(pool, pages_sql),
        bound.fetch::<ChannelUsage>(pool, channels_sql),
        bound.fetch::<RecentRequest>(pool, recent_sql),
    )?;
    let totals = totals
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::internal("пустой итог статистики"))?;
    Ok(Json(Stats {
        days,
        totals,
        daily,
        libraries,
        keys,
        users,
        queries,
        pages,
        channels,
        recent,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_is_read_from_gateway_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            Caller::from_headers(&headers),
            Caller {
                user_id: None,
                key_id: None,
                channel: "api"
            }
        );
        headers.insert(USER_HEADER, "7".parse().unwrap());
        headers.insert(KEY_HEADER, "12".parse().unwrap());
        headers.insert(CHANNEL_HEADER, "mcp".parse().unwrap());
        assert_eq!(
            Caller::from_headers(&headers),
            Caller {
                user_id: Some(7),
                key_id: Some(12),
                channel: "mcp"
            }
        );
        headers.insert(USER_HEADER, "x".parse().unwrap());
        headers.insert(CHANNEL_HEADER, "evil".parse().unwrap());
        let caller = Caller::from_headers(&headers);
        assert_eq!(caller.user_id, None);
        assert_eq!(caller.channel, "api");
    }

    #[test]
    fn docs_record_counts_size_and_unique_pages() {
        let hit = |url: &str, content: &str| SearchHit {
            url: url.into(),
            heading: "Заг".into(),
            content: content.into(),
            rank: 1.0,
        };
        let record = Record::docs(
            "/o/r",
            " routing ",
            &[hit("a", "текст"), hit("b", "x"), hit("a", "yy")],
        );
        assert_eq!(record.query, "routing");
        assert_eq!(record.results, 3);
        assert_eq!(record.chars, (3 + 5) + (3 + 1) + (3 + 2));
        assert_eq!(record.urls, ["a", "b"]);
    }
}
