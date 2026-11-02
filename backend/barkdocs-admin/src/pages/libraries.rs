//! Библиотеки документации: список, добавление, источники и статистика по проекту.
//! Только для администратора.

use axum::{
    Form,
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::json;

use crate::{
    AppState,
    client::{CustomLibrary, KeyScope, NewSource, StatsFilter},
    pages::{failed, optional},
    session::{AdminUser, Flash, FlashKind, PendingFlash, redirect_with},
    views::{self, Ctx, Names, Nav, UsageView, library_href, num, status_badge},
};

#[derive(Deserialize)]
pub struct ListParams {
    q: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    AdminUser(current): AdminUser,
    PendingFlash(flash): PendingFlash,
    Query(params): Query<ListParams>,
) -> Response {
    let q = params.q.as_deref().and_then(optional);
    let libraries = match state.services.libraries(q.as_deref()).await {
        Ok(l) => l,
        Err(e) => return failed(e),
    };
    let count = |status: &str| {
        libraries
            .iter()
            .filter(|l| l.library.status == status)
            .count() as i64
    };
    let chunks: i64 = libraries.iter().map(|l| l.library.chunks).sum();
    let requests: i64 = libraries.iter().map(|l| l.requests).sum();

    views::page(
        Ctx {
            user: &current.user,
            flash,
            nav: Nav::Libraries,
        },
        "Библиотеки",
        html! {
            div.page-head {
                div {
                    h1 { "Библиотеки" }
                    p.muted { "Документация в индексе. Источники проверяются раз в сутки, неизменившиеся не переиндексируются." }
                }
                div.head-actions { a.button href="/libraries/new" { "Добавить библиотеку" } }
            }
            section.tiles {
                (views::tile("Библиотек", &num(libraries.len() as i64), q.as_ref().map(|_| "по фильтру")))
                (views::tile("Готовы", &num(count("ready")), None))
                (views::tile("Индексируются", &num(count("indexing")), None))
                (views::tile("С ошибкой", &num(count("failed")), None))
                (views::tile("Фрагментов в индексе", &num(chunks), None))
                (views::tile("Запросов документации", &num(requests), Some("за всё время")))
            }
            section.card {
                form.inline-form method="get" action="/libraries" role="search" {
                    input type="search" name="q" value=[q.as_deref()] placeholder="Поиск по названию, id или описанию" aria-label="Поиск библиотек";
                    button.button.secondary type="submit" { "Найти" }
                    @if q.is_some() { a.button.ghost href="/libraries" { "Сбросить" } }
                }
                @if libraries.is_empty() {
                    p.empty { "Библиотек не найдено. " a href="/libraries/new" { "Добавить первую" } }
                } @else {
                    div.table-wrap { table {
                        thead { tr {
                            th { "Библиотека" } th { "Статус" } th.num { "Фрагментов" } th.num { "Источников" }
                            th.num { "★" } th.num { "Запросов" } th { "Последний запрос" } th { "Синхронизация" }
                        } }
                        tbody { @for item in &libraries {
                            @let l = &item.library;
                            tr {
                                td {
                                    a href=(library_href(&l.library_id)) { strong { (l.name) } }
                                    div.mono.muted { (l.library_id) }
                                }
                                td { (status_badge(&l.status)) }
                                td.num { (num(l.chunks)) }
                                td.num { (num(item.sources)) }
                                td.num { (num(l.stars)) }
                                td.num { (num(item.requests)) }
                                td { (views::opt_time(item.last_requested_at.as_deref())) }
                                td { (views::opt_time(l.last_synced_at.as_deref())) }
                            }
                        } }
                    } }
                }
            }
        },
    )
}

#[derive(Default, Deserialize)]
pub struct CustomForm {
    slug: String,
    name: String,
    description: String,
    aliases: String,
    sources: String,
}

fn new_page(
    current: &AdminUser,
    flash: Option<Flash>,
    auto_target: &str,
    custom: &CustomForm,
) -> Response {
    views::page(
        Ctx {
            user: &current.0.user,
            flash,
            nav: Nav::Libraries,
        },
        "Добавить библиотеку",
        html! {
            div.page-head {
                div {
                    p.crumbs { a href="/libraries" { "Библиотеки" } " / " }
                    h1 { "Добавить библиотеку" }
                }
            }
            div.grid-2 {
                section.card {
                    h2 { "Автоматически" }
                    p.muted {
                        "Укажите имя пакета npm, crates.io или PyPI — движок найдёт репозиторий и сайт документации. "
                        "Или вставьте ссылку на GitHub-репозиторий либо сайт документации."
                    }
                    form.stack method="post" action="/libraries/new/auto" {
                        label { "Пакет или ссылка"
                            input name="target" value=(auto_target) placeholder="react, serde, https://github.com/tokio-rs/axum" required;
                        }
                        button.button type="submit" { "Найти и добавить" }
                    }
                }
                section.card {
                    h2 { "Вручную" }
                    p.muted { "Полный контроль: свой id, название и точный список источников. Поиск в реестрах не выполняется." }
                    form.stack method="post" action="/libraries/new/custom" {
                        label { "Источники — по одному на строку"
                            textarea name="sources" rows="5" required placeholder="https://github.com/owner/repo\nllms https://example.dev/llms-full.txt\nsite https://example.dev/docs/" { (custom.sources) }
                        }
                        div.hint {
                            "Тип определяется по ссылке, но его можно указать перед ней: "
                            code { "github" } " — README и каталоги docs/ репозитория; "
                            code { "llms" } " — файл llms.txt или llms-full.txt; "
                            code { "site" } " — обход сайта документации от указанной страницы."
                        }
                        label { "Название" input name="name" value=(custom.name) maxlength="200" placeholder="Возьмётся из репозитория или домена"; }
                        label { "Id библиотеки" input name="slug" value=(custom.slug) maxlength="200" placeholder="/owner/name — по умолчанию из первого источника"; }
                        label { "Описание" textarea name="description" rows="2" maxlength="2000" { (custom.description) } }
                        label { "Алиасы через запятую"
                            input name="aliases" value=(custom.aliases) placeholder="имена, по которым агент найдёт библиотеку точным совпадением";
                        }
                        button.button type="submit" { "Создать библиотеку" }
                    }
                }
            }
        },
    )
}

pub async fn new_form(current: AdminUser, PendingFlash(flash): PendingFlash) -> Response {
    new_page(&current, flash, "", &CustomForm::default())
}

#[derive(Deserialize)]
pub struct AutoForm {
    target: String,
}

fn looks_like_link(target: &str) -> bool {
    target.contains("://") || target.starts_with("github.com/") || target.starts_with("www.")
}

pub async fn create_auto(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: AdminUser,
    Form(form): Form<AutoForm>,
) -> Response {
    let target = form.target.trim();
    let body = if looks_like_link(target) {
        let url = if target.contains("://") {
            target.to_string()
        } else {
            format!("https://{target}")
        };
        json!({ "url": url })
    } else {
        json!({ "name": target })
    };
    match state.services.add_library(body).await {
        Ok(libraries) => {
            let names: Vec<&str> = libraries.iter().map(|l| l.library_id.as_str()).collect();
            let message = format!(
                "Добавлено: {}. Документация скачивается в фоне.",
                names.join(", ")
            );
            let to = match libraries.as_slice() {
                [one] => library_href(&one.library_id),
                _ => "/libraries".to_string(),
            };
            redirect_with(&headers, &state, &to, FlashKind::Ok, &message)
        }
        Err(err) => {
            let flash = Some(Flash {
                kind: FlashKind::Error,
                message: err.message,
            });
            new_page(&current, flash, target, &CustomForm::default())
        }
    }
}

/// Строка «[тип] ссылка»; строки с # — комментарии.
pub fn parse_sources(raw: &str) -> Vec<NewSource> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.split_once(char::is_whitespace) {
            Some((kind, url)) if matches!(kind, "github" | "llms" | "site" | "auto") => NewSource {
                kind: Some(kind.to_string()),
                url: url.trim().to_string(),
            },
            _ => NewSource {
                kind: None,
                url: line.to_string(),
            },
        })
        .collect()
}

pub fn parse_aliases(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect()
}

pub async fn create_custom(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: AdminUser,
    Form(form): Form<CustomForm>,
) -> Response {
    let library = CustomLibrary {
        slug: optional(&form.slug),
        name: optional(&form.name),
        description: optional(&form.description),
        aliases: parse_aliases(&form.aliases),
        sources: parse_sources(&form.sources),
    };
    match state.services.create_library(&library).await {
        Ok(created) => redirect_with(
            &headers,
            &state,
            &library_href(&created.library_id),
            FlashKind::Ok,
            "Библиотека создана, документация скачивается в фоне.",
        ),
        Err(err) => {
            let flash = Some(Flash {
                kind: FlashKind::Error,
                message: err.message,
            });
            new_page(&current, flash, "", &form)
        }
    }
}

#[derive(Deserialize)]
pub struct DetailParams {
    id: String,
    days: Option<i32>,
    q: Option<String>,
}

fn source_kind(kind: &str) -> &str {
    match kind {
        "github" => "GitHub",
        "llms" => "llms.txt",
        "site" => "сайт",
        other => other,
    }
}

fn source_status(source: &crate::client::Source) -> Markup {
    html! {
        @if source.syncing {
            span.badge.info { "синхронизация" }
        } @else if let Some(error) = &source.last_error {
            span.badge.bad title=(error) { "ошибка" }
        } @else if source.last_checked_at.is_none() {
            span.badge.info { "в очереди" }
        } @else {
            span.badge.good { "в порядке" }
        }
    }
}

pub async fn detail(
    State(state): State<AppState>,
    AdminUser(current): AdminUser,
    PendingFlash(flash): PendingFlash,
    Query(params): Query<DetailParams>,
) -> Response {
    let services = &state.services;
    let days = params.days.unwrap_or(30);
    let query = params.q.as_deref().and_then(optional);
    let filter = StatsFilter {
        library_id: Some(params.id.clone()),
        days,
        ..Default::default()
    };
    let search = async {
        match &query {
            Some(q) => services
                .search_docs(current.user.id, &params.id, q)
                .await
                .map(Some),
            None => Ok(None),
        }
    };
    let (detail, stats, keys, users, search) = tokio::join!(
        services.library(&params.id),
        services.stats(&filter),
        services.keys(&current.token, KeyScope::All),
        services.users(&current.token),
        search
    );
    let detail = match detail {
        Ok(d) => d,
        Err(e) => return failed(e),
    };
    let (stats, keys, users) = match (stats, keys, users) {
        (Ok(s), Ok(k), Ok(u)) => (s, k, u),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return failed(e),
    };
    let mut names = Names::default();
    for key in &keys {
        names.keys.insert(key.id, (key.name.clone(), key.user_id));
    }
    for u in &users {
        names.users.insert(u.user.id, u.user.login.clone());
    }
    let l = &detail.library;
    let id = &l.library_id;
    let here = library_href(id);

    views::page(
        Ctx {
            user: &current.user,
            flash,
            nav: Nav::Libraries,
        },
        &l.name,
        html! {
            div.page-head {
                div {
                    p.crumbs { a href="/libraries" { "Библиотеки" } " / " }
                    h1 { (l.name) " " (status_badge(&l.status)) }
                    p.mono.muted { (id) }
                    @if !l.description.is_empty() { p { (l.description) } }
                }
                div.head-actions {
                    form method="post" action="/library/sync" {
                        input type="hidden" name="id" value=(id);
                        button.button.secondary type="submit" { "Синхронизировать всё" }
                    }
                    form method="post" action="/library/delete"
                        data-confirm=(format!("Удалить {id} вместе с источниками и индексом? Статистика запросов сохранится.")) {
                        input type="hidden" name="id" value=(id);
                        button.button.danger type="submit" { "Удалить" }
                    }
                }
            }
            section.tiles {
                (views::tile("Фрагментов", &num(l.chunks), None))
                (views::tile("Источников", &num(detail.sources.len() as i64), None))
                (views::tile("Звёзд GitHub", &num(l.stars), None))
                (views::text_tile("Синхронизирована", l.last_synced_at.as_deref().map(views::time).unwrap_or("—"), Some("UTC")))
                (views::text_tile("Добавлена", views::time(&detail.created_at), Some("UTC")))
            }
            @if let Some(error) = &l.last_error {
                div.flash.error { "Последняя ошибка: " (error) }
            }
            section.card {
                h2 { "Источники" }
                @if detail.sources.is_empty() {
                    p.empty { "Источников нет — добавьте хотя бы один, иначе искать будет нечего." }
                } @else {
                    div.table-wrap { table {
                        thead { tr {
                            th { "Тип" } th { "Ссылка" } th { "Статус" } th.num { "Фрагментов" }
                            th { "Проверен" } th { "Успешно" } th { "Следующая проверка" } th { "Версия" } th {}
                        } }
                        tbody { @for s in &detail.sources { tr {
                            td { span.chip { (source_kind(&s.kind)) } }
                            td.url { a href=(s.url) target="_blank" rel="noopener noreferrer" { (s.url) } }
                            td {
                                (source_status(s))
                                @if let Some(error) = &s.last_error { div.small.error-text { (error) } }
                            }
                            td.num { (num(s.chunks)) }
                            td { (views::opt_time(s.last_checked_at.as_deref())) }
                            td { (views::opt_time(s.last_success_at.as_deref())) }
                            td.nowrap { (views::time(&s.next_check_at)) }
                            td.mono.small { @if let Some(v) = &s.version { (v.chars().take(12).collect::<String>()) } }
                            td.actions {
                                form method="post" action="/library/sources/sync" {
                                    input type="hidden" name="id" value=(id);
                                    input type="hidden" name="source_id" value=(s.id);
                                    button.button.small.secondary type="submit" { "Обновить" }
                                }
                                form method="post" action="/library/sources/delete"
                                    data-confirm=(format!("Удалить источник {} и его фрагменты?", s.url)) {
                                    input type="hidden" name="id" value=(id);
                                    input type="hidden" name="source_id" value=(s.id);
                                    button.button.small.danger type="submit" { "Удалить" }
                                }
                            }
                        } } }
                    } }
                }
                h3 { "Добавить источник" }
                form.inline-form method="post" action="/library/sources/add" {
                    input type="hidden" name="id" value=(id);
                    select name="kind" aria-label="Тип источника" {
                        option value="auto" { "Тип по ссылке" }
                        option value="github" { "GitHub" }
                        option value="llms" { "llms.txt" }
                        option value="site" { "Сайт" }
                    }
                    input name="url" placeholder="https://…" required aria-label="Ссылка на источник";
                    button.button type="submit" { "Добавить" }
                }
            }
            div.grid-2 {
                section.card {
                    h2 { "Свойства" }
                    form.stack method="post" action="/library/update" {
                        input type="hidden" name="id" value=(id);
                        label { "Название" input name="name" value=(l.name) required maxlength="200"; }
                        label { "Описание" textarea name="description" rows="3" maxlength="2000" { (l.description) } }
                        label { "Алиасы через запятую" input name="aliases" value=(detail.aliases.join(", ")); }
                        button.button type="submit" { "Сохранить" }
                    }
                }
                section.card {
                    h2 { "Проверка поиска" }
                    p.muted { "Тот же поиск, что выполняет агент в query-docs. Запрос засчитывается вам по каналу «веб-консоль»." }
                    form.inline-form method="get" action="/library" role="search" {
                        input type="hidden" name="id" value=(id);
                        input type="search" name="q" value=[query.as_deref()] placeholder="Например: middleware" aria-label="Запрос";
                        button.button.secondary type="submit" { "Искать" }
                    }
                    @match search {
                        Ok(Some(docs)) if docs.results.is_empty() => p.empty { "Ничего не найдено." },
                        Ok(Some(docs)) => {
                            ol.results { @for hit in &docs.results { li {
                                strong { (hit.heading) }
                                div.small { a href=(hit.url) target="_blank" rel="noopener noreferrer" { (hit.url) } }
                                p.excerpt { (hit.content.chars().take(400).collect::<String>()) @if hit.content.chars().count() > 400 { "…" } }
                            } } }
                        },
                        Ok(None) => {},
                        Err(err) => div.flash.error { (err.message) },
                    }
                }
            }
            div.page-head.section-head {
                h2 { "Использование" }
                (views::period_switch(&here, stats.days))
            }
            (views::usage(&stats, &UsageView {
                names: &names,
                admin: true,
                show_libraries: false,
                show_keys: true,
                show_users: true,
            }))
        },
    )
}

#[derive(Deserialize)]
pub struct IdForm {
    id: String,
}

#[derive(Deserialize)]
pub struct UpdateForm {
    id: String,
    name: String,
    description: String,
    aliases: String,
}

#[derive(Deserialize)]
pub struct SourceForm {
    id: String,
    kind: Option<String>,
    url: String,
}

#[derive(Deserialize)]
pub struct SourceIdForm {
    id: String,
    source_id: i64,
}

fn done(
    headers: &HeaderMap,
    state: &AppState,
    to: &str,
    result: crate::client::ServiceResult<()>,
    ok: &str,
) -> Response {
    match result {
        Ok(()) => redirect_with(headers, state, to, FlashKind::Ok, ok),
        Err(err) => redirect_with(headers, state, to, FlashKind::Error, &err.message),
    }
}

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    _: AdminUser,
    Form(form): Form<UpdateForm>,
) -> Response {
    let body = json!({
        "library_id": form.id,
        "name": form.name,
        "description": form.description,
        "aliases": parse_aliases(&form.aliases),
    });
    let result = state.services.update_library(body).await;
    done(
        &headers,
        &state,
        &library_href(&form.id),
        result,
        "Сохранено.",
    )
}

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    _: AdminUser,
    Form(form): Form<IdForm>,
) -> Response {
    match state.services.delete_library(&form.id).await {
        Ok(()) => redirect_with(
            &headers,
            &state,
            "/libraries",
            FlashKind::Ok,
            &format!("Библиотека {} удалена.", form.id),
        ),
        Err(err) => redirect_with(
            &headers,
            &state,
            &library_href(&form.id),
            FlashKind::Error,
            &err.message,
        ),
    }
}

pub async fn sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    _: AdminUser,
    Form(form): Form<IdForm>,
) -> Response {
    let result = state.services.sync_library(&form.id).await;
    done(
        &headers,
        &state,
        &library_href(&form.id),
        result,
        "Синхронизация запущена. Обновите страницу через минуту.",
    )
}

pub async fn add_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    _: AdminUser,
    Form(form): Form<SourceForm>,
) -> Response {
    let kind = form.kind.as_deref().filter(|k| *k != "auto");
    let result = state
        .services
        .add_source(&form.id, kind, form.url.trim())
        .await;
    done(
        &headers,
        &state,
        &library_href(&form.id),
        result,
        "Источник добавлен, скачивание началось.",
    )
}

pub async fn delete_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    _: AdminUser,
    Form(form): Form<SourceIdForm>,
) -> Response {
    let result = state.services.delete_source(form.source_id).await;
    done(
        &headers,
        &state,
        &library_href(&form.id),
        result,
        "Источник удалён.",
    )
}

pub async fn sync_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    _: AdminUser,
    Form(form): Form<SourceIdForm>,
) -> Response {
    let result = state.services.sync_source(form.source_id).await;
    done(
        &headers,
        &state,
        &library_href(&form.id),
        result,
        "Проверка источника запущена.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_textarea_accepts_optional_kind() {
        let parsed = parse_sources(
            "https://github.com/o/r\n\n# комментарий\nllms https://x.dev/llms.txt\n  site   https://x.dev/docs  \nhttps://x.dev/a b",
        );
        let pairs: Vec<(Option<&str>, &str)> = parsed
            .iter()
            .map(|s| (s.kind.as_deref(), s.url.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                (None, "https://github.com/o/r"),
                (Some("llms"), "https://x.dev/llms.txt"),
                (Some("site"), "https://x.dev/docs"),
                (None, "https://x.dev/a b"),
            ]
        );
    }

    #[test]
    fn aliases_split_by_commas_and_lines() {
        assert_eq!(
            parse_aliases("react, reactjs\n react-dom ,"),
            ["react", "reactjs", "react-dom"]
        );
    }

    #[test]
    fn link_detection_for_auto_add() {
        assert!(looks_like_link("https://x.dev"));
        assert!(looks_like_link("github.com/o/r"));
        assert!(!looks_like_link("@scope/pkg"));
        assert!(!looks_like_link("serde"));
    }
}
