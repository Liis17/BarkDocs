//! Ключи MCP: выпуск, отзыв и инструкция подключения агента.

use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, State},
    http::HeaderMap,
    response::Response,
};
use maud::{Markup, html};
use serde::Deserialize;

use crate::{
    AppState,
    client::{CreatedKey, KeyInfo, KeyScope, StatsFilter},
    pages::failed,
    session::{CurrentUser, Flash, FlashKind, PendingFlash, local_path, redirect_with},
    views::{self, Ctx, Nav},
};

/// Таблица ключей; `back` — куда вернуться после отзыва.
pub fn keys_table(
    keys: &[KeyInfo],
    usage: &HashMap<i64, (i64, i64)>,
    back: &str,
    show_owner: bool,
) -> Markup {
    html! {
        @if keys.is_empty() {
            p.empty { "Ключей пока нет." }
        } @else {
            div.table-wrap { table {
                thead { tr {
                    th { "Название" }
                    @if show_owner { th { "Владелец" } }
                    th { "Создан" } th { "Использован" } th.num { "Запросов" } th.num { "≈ токенов" } th { "Статус" } th {}
                } }
                tbody { @for key in keys {
                    @let (requests, chars) = usage.get(&key.id).copied().unwrap_or((0, 0));
                    tr class=[key.revoked_at.is_some().then_some("revoked")] {
                        td { (key.name) " " span.muted.mono { "#" (key.id) } }
                        @if show_owner {
                            td { @match (&key.login, key.user_id) {
                                (Some(login), Some(id)) => a href=(format!("/users/{id}")) { (login) },
                                _ => span.muted { "служебный" },
                            } }
                        }
                        td.nowrap { (views::time(&key.created_at)) }
                        td { (views::opt_time(key.last_used_at.as_deref())) }
                        td.num { (views::num(requests)) }
                        td.num { (views::tokens(chars)) }
                        td { @if key.revoked_at.is_some() { span.badge.bad { "отозван" } } @else { span.badge.good { "активен" } } }
                        td.actions {
                            @if key.revoked_at.is_none() {
                                form method="post" action=(format!("/keys/{}/revoke", key.id))
                                    data-confirm=(format!("Отозвать ключ «{}»? Агент с ним сразу потеряет доступ.", key.name)) {
                                    input type="hidden" name="back" value=(back);
                                    button.button.small.danger type="submit" { "Отозвать" }
                                }
                            }
                        }
                    }
                } }
            } }
        }
    }
}

async fn render(
    state: &AppState,
    current: &CurrentUser,
    flash: Option<Flash>,
    created: Option<CreatedKey>,
) -> Response {
    let services = &state.services;
    let filter = StatsFilter {
        user_id: Some(current.user.id),
        days: 0,
        ..Default::default()
    };
    let (keys, stats) = tokio::join!(
        services.keys(&current.token, KeyScope::Own),
        services.stats(&filter)
    );
    let (keys, stats) = match (keys, stats) {
        (Ok(k), Ok(s)) => (k, s),
        (Err(e), _) | (_, Err(e)) => return failed(e),
    };
    let usage: HashMap<i64, (i64, i64)> = stats
        .keys
        .iter()
        .map(|k| (k.key_id, (k.requests, k.chars)))
        .collect();

    views::page(
        Ctx {
            user: &current.user,
            flash,
            nav: Nav::Keys,
        },
        "Ключи MCP",
        html! {
            div.page-head {
                div {
                    h1 { "Ключи MCP" }
                    p.muted { "Выпустите отдельный ключ каждому агенту: так видно, кто сколько запрашивает, и ключ можно отозвать, не трогая остальных." }
                }
            }
            @if let Some(created) = &created {
                section.card.highlight {
                    h2 { "Ключ «" (created.name) "» создан" }
                    p { strong { "Скопируйте его сейчас" } " — показать ключ ещё раз будет невозможно, хранится только его хеш." }
                    (views::snippet("new-key", &created.key))
                    (views::connect_instructions(&state.mcp_url, Some(&created.key)))
                }
            }
            div.grid-2 {
                section.card {
                    h2 { "Новый ключ" }
                    form.inline-form method="post" action="/keys" {
                        input name="name" placeholder="Например: claude-code ноутбук" required maxlength="100" aria-label="Название ключа";
                        button.button type="submit" { "Выпустить ключ" }
                    }
                }
                @if created.is_none() {
                    section.card {
                        h2 { "Подключение агента" }
                        (views::connect_instructions(&state.mcp_url, None))
                    }
                }
            }
            section.card {
                h2 { "Мои ключи" }
                (keys_table(&keys, &usage, "/keys", false))
            }
        },
    )
}

pub async fn keys_page(
    State(state): State<AppState>,
    current: CurrentUser,
    PendingFlash(flash): PendingFlash,
) -> Response {
    render(&state, &current, flash, None).await
}

#[derive(Deserialize)]
pub struct CreateForm {
    name: String,
}

/// Ключ показывается прямо в ответе на POST, а не после редиректа: так он не попадает
/// ни в URL, ни в cookie.
pub async fn create_key(
    State(state): State<AppState>,
    current: CurrentUser,
    Form(form): Form<CreateForm>,
) -> Response {
    match state.services.create_key(&current.token, &form.name).await {
        Ok(created) => render(&state, &current, None, Some(created)).await,
        Err(err) => {
            let flash = Some(Flash {
                kind: FlashKind::Error,
                message: err.message,
            });
            render(&state, &current, flash, None).await
        }
    }
}

#[derive(Deserialize)]
pub struct RevokeForm {
    back: Option<String>,
}

pub async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: CurrentUser,
    Path(id): Path<i64>,
    Form(form): Form<RevokeForm>,
) -> Response {
    let back = local_path(form.back.as_deref(), "/keys");
    match state.services.revoke_key(&current.token, id).await {
        Ok(()) => redirect_with(&headers, &state, &back, FlashKind::Ok, "Ключ отозван."),
        Err(err) => redirect_with(&headers, &state, &back, FlashKind::Error, &err.message),
    }
}
