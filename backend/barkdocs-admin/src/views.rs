//! Разметка консоли: общий макет, форматирование и блоки статистики.
//! Всё рендерится на сервере; maud экранирует вставляемые значения.

use std::collections::HashMap;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maud::{DOCTYPE, Markup, html};

use crate::{
    client::{Day, Stats, User},
    session::{Flash, FlashKind},
};

pub fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Числа с разделителем разрядов: 12 345.
pub fn num(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    if value < 0 { format!("-{out}") } else { out }
}

/// Объём ответа в токенах: оценка «4 символа на токен», как у большинства токенизаторов.
pub fn tokens(chars: i64) -> String {
    num((chars + 3) / 4)
}

/// Время из БД без секунд и часового пояса: `2026-11-02 10:15`.
pub fn time(value: &str) -> &str {
    value.get(..16).unwrap_or(value)
}

pub fn opt_time(value: Option<&str>) -> Markup {
    match value {
        Some(v) => html! { span.nowrap { (time(v)) } },
        None => html! { span.muted { "—" } },
    }
}

pub fn library_href(id: &str) -> String {
    format!("/library?id={}", encode(id))
}

pub fn status_badge(status: &str) -> Markup {
    let (class, label) = match status {
        "ready" => ("badge good", "готово"),
        "indexing" => ("badge info", "индексируется"),
        "failed" => ("badge bad", "ошибка"),
        other => ("badge", other),
    };
    html! { span class=(class) { (label) } }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Nav {
    Overview,
    Keys,
    Libraries,
    Users,
    Account,
}

pub struct Ctx<'a> {
    pub user: &'a User,
    pub flash: Option<Flash>,
    pub nav: Nav,
}

fn flash_block(flash: &Option<Flash>) -> Markup {
    html! {
        @if let Some(flash) = flash {
            @let class = if flash.kind == FlashKind::Ok { "flash ok" } else { "flash error" };
            div class=(class) role="status" { (flash.message) }
        }
    }
}

fn document(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="ru" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · BarkDocs" }
                link rel="stylesheet" href="/static/app.css";
                link rel="icon" href="/static/favicon.svg" type="image/svg+xml";
                script src="/static/app.js" defer {}
            }
            body { (body) }
        }
    }
}

pub fn page(ctx: Ctx<'_>, title: &str, content: Markup) -> Response {
    let user = ctx.user;
    let item = |nav: Nav, href: &str, label: &str| {
        html! {
            @if ctx.nav == nav {
                a.active href=(href) aria-current="page" { (label) }
            } @else {
                a href=(href) { (label) }
            }
        }
    };
    let body = html! {
        header.topbar {
            div.topbar-inner {
                a.brand href="/" { span.brand-mark { "B" } "BarkDocs" }
                nav.nav {
                    (item(Nav::Overview, "/", "Обзор"))
                    (item(Nav::Keys, "/keys", "Ключи MCP"))
                    @if user.is_admin() {
                        (item(Nav::Libraries, "/libraries", "Библиотеки"))
                        (item(Nav::Users, "/users", "Пользователи"))
                    }
                }
                div.account {
                    a.account-link href="/account" {
                        (user.login)
                        @if user.is_admin() { " " span.badge.admin { "admin" } }
                    }
                    form method="post" action="/logout" {
                        button.link type="submit" { "Выйти" }
                    }
                }
            }
        }
        main.container {
            (flash_block(&ctx.flash))
            (content)
        }
    };
    document(title, body).into_response()
}

/// Страница без навигации: вход, регистрация.
pub fn bare_page(title: &str, flash: Option<Flash>, content: Markup) -> Response {
    let body = html! {
        main.auth-container {
            a.brand.center href="/" { span.brand-mark { "B" } "BarkDocs" }
            (flash_block(&flash))
            (content)
        }
    };
    document(title, body).into_response()
}

pub fn error_page(status: StatusCode, message: &str) -> Response {
    let body = html! {
        main.auth-container {
            a.brand.center href="/" { span.brand-mark { "B" } "BarkDocs" }
            div.card {
                h1 { "Ошибка " (status.as_u16()) }
                p { (message) }
                p { a href="/" { "На главную" } }
            }
        }
    };
    (status, document("Ошибка", body)).into_response()
}

pub fn tile(label: &str, value: &str, hint: Option<&str>) -> Markup {
    tile_with(label, value, hint, "tile-value")
}

/// Плитка с датой или текстом вместо числа: шрифт меньше, чтобы значение не переносилось.
pub fn text_tile(label: &str, value: &str, hint: Option<&str>) -> Markup {
    tile_with(label, value, hint, "tile-value text")
}

fn tile_with(label: &str, value: &str, hint: Option<&str>, class: &str) -> Markup {
    html! {
        div.tile {
            div.tile-label { (label) }
            div class=(class) { (value) }
            @if let Some(hint) = hint { div.tile-hint { (hint) } }
        }
    }
}

/// Доля от максимума для CSS-классов `.h-N` / `.w-N`: инлайн-стили запрещены CSP.
fn percent(value: i64, max: i64) -> i64 {
    if value <= 0 || max <= 0 {
        0
    } else {
        ((value * 100 + max - 1) / max).clamp(1, 100)
    }
}

fn axis_max(max: i64) -> i64 {
    if max <= 5 {
        return max.max(1);
    }
    let magnitude = 10_i64.pow((max as f64).log10().floor() as u32);
    for step in [1, 2, 5, 10] {
        if max <= step * magnitude {
            return step * magnitude;
        }
    }
    max
}

/// Столбики запросов по дням: одна серия, подсказка при наведении и фокусе.
pub fn daily_chart(days: &[Day]) -> Markup {
    let max = axis_max(days.iter().map(|d| d.requests).max().unwrap_or(0));
    let total: i64 = days.iter().map(|d| d.requests).sum();
    let first = days.first().map(|d| d.day.as_str()).unwrap_or_default();
    let last = days.last().map(|d| d.day.as_str()).unwrap_or_default();
    html! {
        figure.chart-figure {
            div.chart-plot {
                div.chart-axis aria-hidden="true" {
                    span { (num(max)) }
                    span { (num(max / 2)) }
                    span { "0" }
                }
                div.chart role="list" aria-label=(format!("Запросы по дням, всего {}", num(total))) {
                    @for day in days {
                        @let label = format!("{}: запросов {}, документации {}", day.day, num(day.requests), num(day.docs));
                        div.col role="listitem" tabindex="0" aria-label=(label) {
                            div class=(format!("bar h-{}", percent(day.requests, max))) {}
                            div.tip {
                                strong { (day.day) }
                                br;
                                "Запросов: " (num(day.requests))
                                br;
                                "Документации: " (num(day.docs))
                            }
                        }
                    }
                }
            }
            div.chart-x aria-hidden="true" {
                span { (first) }
                span { (last) }
            }
            details.table-view {
                summary { "Таблица по дням" }
                div.table-wrap {
                    table {
                        thead { tr { th { "День" } th.num { "Запросов" } th.num { "Документации" } } }
                        tbody {
                            @for day in days.iter().rev() {
                                tr { td { (day.day) } td.num { (num(day.requests)) } td.num { (num(day.docs)) } }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Полоска доли в ячейке таблицы, рядом всегда стоит число.
pub fn share(value: i64, max: i64) -> Markup {
    html! {
        span.share aria-hidden="true" { span class=(format!("share-fill w-{}", percent(value, max))) {} }
    }
}

pub const PERIODS: [(i32, &str); 5] = [
    (7, "7 дней"),
    (30, "30 дней"),
    (90, "90 дней"),
    (365, "год"),
    (0, "всё время"),
];

pub fn period_label(days: i32) -> &'static str {
    match days {
        7 => "за 7 дней",
        30 => "за 30 дней",
        90 => "за 90 дней",
        365 => "за год",
        0 => "за всё время",
        _ => "за период",
    }
}

/// Переключатель периода; `base` — адрес страницы с параметрами, к которому добавляется days.
pub fn period_switch(base: &str, current: i32) -> Markup {
    let sep = if base.contains('?') { "&" } else { "?" };
    html! {
        nav.segmented aria-label="Период" {
            @for (days, label) in PERIODS {
                @let href = format!("{base}{sep}days={days}");
                @if days == current {
                    a.active href=(href) aria-current="true" { (label) }
                } @else {
                    a href=(href) { (label) }
                }
            }
        }
    }
}

/// Что известно консоли о ключах и пользователях для подписей в статистике.
#[derive(Default)]
pub struct Names {
    pub keys: HashMap<i64, (String, Option<i64>)>,
    pub users: HashMap<i64, String>,
}

impl Names {
    fn key(&self, id: i64) -> String {
        self.keys
            .get(&id)
            .map_or_else(|| format!("ключ #{id}"), |(name, _)| name.clone())
    }

    fn user(&self, id: i64) -> Markup {
        let login = self
            .users
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("#{id}"));
        html! { a href=(format!("/users/{id}")) { (login) } }
    }
}

pub struct UsageView<'a> {
    pub names: &'a Names,
    /// Видны ли разбивки по пользователям и ссылки на страницы администратора.
    pub admin: bool,
    pub show_libraries: bool,
    pub show_keys: bool,
    pub show_users: bool,
}

fn channel_label(channel: &str) -> &str {
    match channel {
        "mcp" => "MCP",
        "api" => "HTTP API",
        "web" => "веб-консоль",
        other => other,
    }
}

fn library_cell(id: &str, admin: bool) -> Markup {
    html! {
        @if admin { a.mono href=(library_href(id)) { (id) } } @else { span.mono { (id) } }
    }
}

/// Счётчики, график и разбивки по библиотекам, ключам, пользователям, запросам и страницам.
pub fn usage(stats: &Stats, view: &UsageView<'_>) -> Markup {
    let t = &stats.totals;
    let period = period_label(stats.days);
    let max_library = stats
        .libraries
        .iter()
        .map(|l| l.requests)
        .max()
        .unwrap_or(0);
    let max_key = stats.keys.iter().map(|k| k.requests).max().unwrap_or(0);
    let max_user = stats.users.iter().map(|u| u.requests).max().unwrap_or(0);
    html! {
        section.tiles {
            (tile("Всего запросов", &num(t.requests), t.last_at.as_deref().map(|l| format!("последний {}", time(l))).as_deref()))
            (tile(&format!("Запросов {period}"), &num(t.in_period), None))
            (tile("Сегодня", &num(t.today), Some("по UTC")))
            (tile("Запросов документации", &num(t.docs), Some("query-docs")))
            (tile("Поиск библиотек", &num(t.resolves), Some("resolve-library-id")))
            (tile("Отдано токенов", &format!("≈ {}", tokens(t.chars)), Some(&format!("{} фрагментов", num(t.results)))))
            @if view.show_users { (tile("Пользователей", &num(t.users), Some("с запросами"))) }
            @if view.show_libraries { (tile("Библиотек", &num(t.libraries), Some("запрошено"))) }
        }
        section.card {
            div.card-head {
                h2 { "Запросы по дням" }
                @if !stats.channels.is_empty() {
                    div.chips {
                        @for c in &stats.channels {
                            span.chip { (channel_label(&c.channel)) ": " strong { (num(c.requests)) } }
                        }
                    }
                }
            }
            (daily_chart(&stats.daily))
        }
        div.grid-2 {
            @if view.show_libraries {
                section.card {
                    h2 { "Библиотеки " span.muted { (period) } }
                    @if stats.libraries.is_empty() {
                        p.empty { "Запросов документации пока не было." }
                    } @else {
                        div.table-wrap { table {
                            thead { tr {
                                th { "Библиотека" } th.num { "Запросов" } th {}
                                th.num { "≈ токенов" }
                                @if view.show_users { th.num { "Польз." } }
                                th { "Последний" }
                            } }
                            tbody { @for l in &stats.libraries { tr {
                                td { (library_cell(&l.library, view.admin)) }
                                td.num { (num(l.requests)) }
                                td.share-cell { (share(l.requests, max_library)) }
                                td.num { (tokens(l.chars)) }
                                @if view.show_users { td.num { (num(l.users)) } }
                                td.nowrap { (time(&l.last_at)) }
                            } } }
                        } }
                    }
                }
            }
            @if view.show_keys {
                section.card {
                    h2 { "Агенты (ключи) " span.muted { (period) } }
                    @if stats.keys.is_empty() {
                        p.empty { "Ключи ещё не использовались." }
                    } @else {
                        div.table-wrap { table {
                            thead { tr {
                                th { "Ключ" }
                                @if view.show_users { th { "Владелец" } }
                                th.num { "Запросов" } th {} th.num { "Доки" } th.num { "≈ токенов" } th { "Последний" }
                            } }
                            tbody { @for k in &stats.keys { tr {
                                td { (view.names.key(k.key_id)) }
                                @if view.show_users {
                                    td { @match k.user_id { Some(id) => (view.names.user(id)), None => span.muted { "без владельца" } } }
                                }
                                td.num { (num(k.requests)) }
                                td.share-cell { (share(k.requests, max_key)) }
                                td.num { (num(k.docs)) }
                                td.num { (tokens(k.chars)) }
                                td.nowrap { (time(&k.last_at)) }
                            } } }
                        } }
                    }
                }
            }
            @if view.show_users {
                section.card {
                    h2 { "Пользователи " span.muted { (period) } }
                    @if stats.users.is_empty() {
                        p.empty { "Запросов от пользователей пока не было." }
                    } @else {
                        div.table-wrap { table {
                            thead { tr { th { "Пользователь" } th.num { "Запросов" } th {} th.num { "Доки" } th.num { "≈ токенов" } th { "Последний" } } }
                            tbody { @for u in &stats.users { tr {
                                td { (view.names.user(u.user_id)) }
                                td.num { (num(u.requests)) }
                                td.share-cell { (share(u.requests, max_user)) }
                                td.num { (num(u.docs)) }
                                td.num { (tokens(u.chars)) }
                                td.nowrap { (time(&u.last_at)) }
                            } } }
                        } }
                    }
                }
            }
            section.card {
                h2 { "Частые запросы " span.muted { (period) } }
                @if stats.queries.is_empty() {
                    p.empty { "Пока пусто." }
                } @else {
                    div.table-wrap { table {
                        thead { tr { th { "Запрос" } @if view.show_libraries { th { "Библиотека" } } th.num { "Раз" } } }
                        tbody { @for q in &stats.queries { tr {
                            td { (q.query) }
                            @if view.show_libraries {
                                td { @if let Some(l) = &q.library { (library_cell(l, view.admin)) } }
                            }
                            td.num { (num(q.count)) }
                        } } }
                    } }
                }
            }
            section.card {
                h2 { "Самые выдаваемые страницы " span.muted { (period) } }
                @if stats.pages.is_empty() {
                    p.empty { "Пока пусто." }
                } @else {
                    div.table-wrap { table {
                        thead { tr { th { "Страница документации" } th.num { "Раз" } } }
                        tbody { @for p in &stats.pages { tr {
                            td.url { a href=(p.url) target="_blank" rel="noopener noreferrer" { (p.url) } }
                            td.num { (num(p.count)) }
                        } } }
                    } }
                }
            }
        }
        section.card {
            h2 { "Последние запросы" }
            @if stats.recent.is_empty() {
                p.empty { "Запросов ещё не было. Подключите агента по ключу из раздела «Ключи MCP»." }
            } @else {
                div.table-wrap { table {
                    thead { tr {
                        th { "Время, UTC" }
                        @if view.show_users { th { "Пользователь" } }
                        th { "Ключ" } th { "Канал" } th { "Тип" } th { "Библиотека" } th { "Запрос" } th.num { "Найдено" } th.num { "≈ токенов" }
                    } }
                    tbody { @for r in &stats.recent { tr {
                        td.nowrap { (r.at) }
                        @if view.show_users {
                            td { @match r.user_id { Some(id) => (view.names.user(id)), None => span.muted { "—" } } }
                        }
                        td { @match r.key_id { Some(id) => (view.names.key(id)), None => span.muted { "—" } } }
                        td { (channel_label(&r.channel)) }
                        td { @if r.kind == "docs" { "документация" } @else { "поиск библиотеки" } }
                        td { @if let Some(l) = &r.library { (library_cell(l, view.admin)) } @else { span.muted { "—" } } }
                        td.query { (r.query) }
                        td.num { (r.results) }
                        td.num { (tokens(r.chars)) }
                    } } }
                } }
            }
        }
    }
}

/// Инструкция подключения агента; `key` подставляется, если ключ только что выпущен.
pub fn connect_instructions(mcp_url: &str, key: Option<&str>) -> Markup {
    let key = key.unwrap_or("<ВАШ_КЛЮЧ>");
    let claude = format!(
        "claude mcp add --transport http barkdocs {mcp_url} --header \"Authorization: Bearer {key}\""
    );
    let json = format!(
        "{{\n  \"mcpServers\": {{\n    \"barkdocs\": {{\n      \"url\": \"{mcp_url}\",\n      \"headers\": {{ \"Authorization\": \"Bearer {key}\" }}\n    }}\n  }}\n}}"
    );
    let curl = format!(
        "curl -s {mcp_url} -H \"Authorization: Bearer {key}\" -H \"Content-Type: application/json\" \\\n  -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}'"
    );
    html! {
        div.instructions {
            p { "Адрес MCP-сервера (Streamable HTTP): " code { (mcp_url) } }
            h3 { "Claude Code" }
            (snippet("snippet-claude", &claude))
            h3 { "Cursor, Windsurf и другие клиенты с JSON-конфигом" }
            (snippet("snippet-json", &json))
            h3 { "Проверка из терминала" }
            (snippet("snippet-curl", &curl))
            p.muted {
                "Агенту доступны инструменты " code { "resolve-library-id" } " и " code { "query-docs" }
                ". Лимитов на число запросов нет; всё использование видно на странице «Обзор»."
            }
        }
    }
}

pub fn snippet(id: &str, text: &str) -> Markup {
    html! {
        div.snippet {
            pre id=(id) { code { (text) } }
            button.button.small.secondary type="button" data-copy=(id) { "Копировать" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_get_digit_groups() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(1000), "1\u{202F}000");
        assert_eq!(num(-1234567), "-1\u{202F}234\u{202F}567");
        assert_eq!(tokens(9), "3");
    }

    #[test]
    fn chart_scale_is_rounded_and_marks_nonzero_days() {
        assert_eq!(axis_max(0), 1);
        assert_eq!(axis_max(4), 4);
        assert_eq!(axis_max(7), 10);
        assert_eq!(axis_max(130), 200);
        assert_eq!(axis_max(4000), 5000);
        assert_eq!(percent(0, 10), 0);
        assert_eq!(percent(1, 1000), 1);
        assert_eq!(percent(10, 10), 100);
    }

    #[test]
    fn time_is_trimmed_to_minutes() {
        assert_eq!(time("2026-11-02 10:15:42.1+00"), "2026-11-02 10:15");
        assert_eq!(time("short"), "short");
    }
}
