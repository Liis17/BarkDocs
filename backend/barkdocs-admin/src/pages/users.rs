//! Пользователи и их потребление. Только для администратора.

use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Response,
};
use maud::html;
use serde::Deserialize;
use serde_json::json;

use crate::{
    AppState,
    client::{KeyScope, StatsFilter},
    pages::{failed, keys::keys_table},
    session::{AdminUser, FlashKind, PendingFlash, redirect_with},
    views::{self, Ctx, Names, Nav, UsageView, num},
};

#[derive(Deserialize)]
pub struct PeriodParams {
    days: Option<i32>,
}

fn role_badge(role: &str) -> maud::Markup {
    html! {
        @if role == "admin" { span.badge.admin { "администратор" } } @else { span.badge { "пользователь" } }
    }
}

pub async fn list(
    State(state): State<AppState>,
    AdminUser(current): AdminUser,
    PendingFlash(flash): PendingFlash,
    Query(params): Query<PeriodParams>,
) -> Response {
    let services = &state.services;
    let days = params.days.unwrap_or(30);
    let period = StatsFilter {
        days,
        ..Default::default()
    };
    let all_time = StatsFilter {
        days: 0,
        ..Default::default()
    };
    let (users, stats, totals) = tokio::join!(
        services.users(&current.token),
        services.stats(&period),
        services.stats(&all_time)
    );
    let (users, stats, totals) = match (users, stats, totals) {
        (Ok(u), Ok(s), Ok(t)) => (u, s, t),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return failed(e),
    };
    let in_period: HashMap<i64, _> = stats.users.iter().map(|u| (u.user_id, u)).collect();
    let ever: HashMap<i64, _> = totals.users.iter().map(|u| (u.user_id, u)).collect();
    let max = stats.users.iter().map(|u| u.requests).max().unwrap_or(0);
    let active = stats.users.len() as i64;
    let disabled = users.iter().filter(|u| u.user.disabled).count() as i64;
    let label = views::period_label(stats.days);

    views::page(
        Ctx {
            user: &current.user,
            flash,
            nav: Nav::Users,
        },
        "Пользователи",
        html! {
            div.page-head {
                div {
                    h1 { "Пользователи" }
                    p.muted { "Аккаунты консоли и потребление их агентов." }
                }
                div.head-actions { (views::period_switch("/users", stats.days)) }
            }
            section.tiles {
                (views::tile("Пользователей", &num(users.len() as i64), None))
                (views::tile(&format!("Активных {label}"), &num(active), Some("делали запросы")))
                (views::tile("Заблокировано", &num(disabled), None))
                (views::tile("Активных ключей", &num(users.iter().map(|u| u.active_keys).sum()), None))
            }
            section.card {
                div.table-wrap { table {
                    thead { tr {
                        th { "Логин" } th { "Роль" } th { "Статус" } th { "Создан" } th.num { "Ключей" }
                        th.num { "Запросов " (label) } th {} th.num { "≈ токенов " (label) }
                        th.num { "Всего запросов" } th { "Последний запрос" }
                    } }
                    tbody { @for summary in &users {
                        @let u = &summary.user;
                        @let p = in_period.get(&u.id);
                        @let e = ever.get(&u.id);
                        tr class=[u.disabled.then_some("revoked")] {
                            td { a href=(format!("/users/{}", u.id)) { strong { (u.login) } } }
                            td { (role_badge(&u.role)) }
                            td { @if u.disabled { span.badge.bad { "заблокирован" } } @else { span.badge.good { "активен" } } }
                            td.nowrap { (views::time(&u.created_at)) }
                            td.num { (num(summary.active_keys)) }
                            td.num { (num(p.map_or(0, |p| p.requests))) }
                            td.share-cell { (views::share(p.map_or(0, |p| p.requests), max)) }
                            td.num { (views::tokens(p.map_or(0, |p| p.chars))) }
                            td.num { (num(e.map_or(0, |e| e.requests))) }
                            td { (views::opt_time(e.map(|e| e.last_at.as_str()))) }
                        }
                    } }
                } }
            }
        },
    )
}

pub async fn detail(
    State(state): State<AppState>,
    AdminUser(current): AdminUser,
    PendingFlash(flash): PendingFlash,
    Path(id): Path<i64>,
    Query(params): Query<PeriodParams>,
) -> Response {
    let services = &state.services;
    let days = params.days.unwrap_or(30);
    let filter = StatsFilter {
        user_id: Some(id),
        days,
        ..Default::default()
    };
    let all_time = StatsFilter {
        user_id: Some(id),
        days: 0,
        ..Default::default()
    };
    let (user, keys, stats, totals) = tokio::join!(
        services.user(&current.token, id),
        services.keys(&current.token, KeyScope::User(id)),
        services.stats(&filter),
        services.stats(&all_time)
    );
    let (summary, keys, stats, totals) = match (user, keys, stats, totals) {
        (Ok(u), Ok(k), Ok(s), Ok(t)) => (u, k, s, t),
        (Err(e), ..) | (_, Err(e), ..) | (_, _, Err(e), _) | (.., Err(e)) => return failed(e),
    };
    let u = &summary.user;
    let mut names = Names::default();
    for key in &keys {
        names.keys.insert(key.id, (key.name.clone(), key.user_id));
    }
    names.users.insert(u.id, u.login.clone());
    let usage: HashMap<i64, (i64, i64)> = totals
        .keys
        .iter()
        .map(|k| (k.key_id, (k.requests, k.chars)))
        .collect();
    let here = format!("/users/{id}");
    let is_self = u.id == current.user.id;

    views::page(
        Ctx {
            user: &current.user,
            flash,
            nav: Nav::Users,
        },
        &u.login,
        html! {
            div.page-head {
                div {
                    p.crumbs { a href="/users" { "Пользователи" } " / " }
                    h1 { (u.login) " " (role_badge(&u.role)) @if u.disabled { " " span.badge.bad { "заблокирован" } } }
                    p.muted { "Создан " (views::time(&u.created_at)) " UTC" }
                }
                @if !is_self {
                    div.head-actions {
                        form method="post" action=(format!("/users/{id}/update")) {
                            @if u.disabled {
                                input type="hidden" name="action" value="enable";
                                button.button.secondary type="submit" { "Разблокировать" }
                            } @else {
                                input type="hidden" name="action" value="disable";
                                button.button.danger type="submit" data-confirm-button="Заблокировать пользователя? Его ключи и сессии перестанут работать." { "Заблокировать" }
                            }
                        }
                        form method="post" action=(format!("/users/{id}/update")) {
                            @if u.is_admin() {
                                input type="hidden" name="action" value="make_user";
                                button.button.secondary type="submit" { "Снять роль администратора" }
                            } @else {
                                input type="hidden" name="action" value="make_admin";
                                button.button.secondary type="submit" data-confirm-button="Дать пользователю полный доступ администратора?" { "Сделать администратором" }
                            }
                        }
                    }
                }
            }
            section.card {
                h2 { "Ключи" }
                (keys_table(&keys, &usage, &here, false))
            }
            div.page-head.section-head {
                h2 { "Использование" }
                (views::period_switch(&here, stats.days))
            }
            (views::usage(&stats, &UsageView {
                names: &names,
                admin: true,
                show_libraries: true,
                show_keys: true,
                show_users: false,
            }))
        },
    )
}

#[derive(Deserialize)]
pub struct UpdateForm {
    action: String,
}

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    AdminUser(current): AdminUser,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> Response {
    let (change, message) = match form.action.as_str() {
        "disable" => (json!({ "disabled": true }), "Пользователь заблокирован."),
        "enable" => (json!({ "disabled": false }), "Пользователь разблокирован."),
        "make_admin" => (
            json!({ "role": "admin" }),
            "Пользователь стал администратором.",
        ),
        "make_user" => (json!({ "role": "user" }), "Роль администратора снята."),
        _ => {
            return redirect_with(
                &headers,
                &state,
                "/users",
                FlashKind::Error,
                "Неизвестное действие.",
            );
        }
    };
    let back = format!("/users/{id}");
    match state.services.update_user(&current.token, id, change).await {
        Ok(()) => redirect_with(&headers, &state, &back, FlashKind::Ok, message),
        Err(err) => redirect_with(&headers, &state, &back, FlashKind::Error, &err.message),
    }
}
