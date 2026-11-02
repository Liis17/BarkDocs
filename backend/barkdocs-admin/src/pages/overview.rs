//! Обзор использования: пользователь видит свои ключи, администратор — всю систему
//! или только свой аккаунт.

use axum::{
    extract::{Query, State},
    response::Response,
};
use maud::html;
use serde::Deserialize;

use crate::{
    AppState,
    client::{KeyScope, StatsFilter},
    pages::failed,
    session::{CurrentUser, PendingFlash},
    views::{self, Ctx, Names, Nav, UsageView},
};

#[derive(Deserialize)]
pub struct OverviewParams {
    days: Option<i32>,
    scope: Option<String>,
}

pub async fn overview(
    State(state): State<AppState>,
    current: CurrentUser,
    PendingFlash(flash): PendingFlash,
    Query(params): Query<OverviewParams>,
) -> Response {
    let user = &current.user;
    let days = params.days.unwrap_or(30);
    let everyone = user.is_admin() && params.scope.as_deref() != Some("me");
    let services = &state.services;

    let filter = StatsFilter {
        user_id: (!everyone).then_some(user.id),
        days,
        ..Default::default()
    };
    let key_scope = if everyone {
        KeyScope::All
    } else {
        KeyScope::Own
    };
    let users = async {
        if everyone {
            services.users(&current.token).await.map(Some)
        } else {
            Ok(None)
        }
    };
    let (stats, keys, users) = tokio::join!(
        services.stats(&filter),
        services.keys(&current.token, key_scope),
        users
    );
    let (stats, keys, users) = match (stats, keys, users) {
        (Ok(s), Ok(k), Ok(u)) => (s, k, u),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return failed(e),
    };

    let mut names = Names::default();
    for key in &keys {
        names.keys.insert(key.id, (key.name.clone(), key.user_id));
    }
    for summary in users.iter().flatten() {
        names
            .users
            .insert(summary.user.id, summary.user.login.clone());
    }
    let base = if everyone {
        "/"
    } else if user.is_admin() {
        "/?scope=me"
    } else {
        "/"
    };
    let active_keys = keys.iter().filter(|k| k.revoked_at.is_none()).count();

    views::page(
        Ctx {
            user,
            flash,
            nav: Nav::Overview,
        },
        "Обзор",
        html! {
            div.page-head {
                div {
                    h1 { @if everyone { "Использование всей системы" } @else { "Моё использование" } }
                    p.muted {
                        @if everyone {
                            "Все запросы агентов всех пользователей. Лимитов нет — счётчики только для учёта."
                        } @else {
                            "Запросы ваших агентов по их ключам. Активных ключей: " (active_keys) ". "
                            a href="/keys" { "Управление ключами" }
                        }
                    }
                }
                div.head-actions {
                    @if user.is_admin() {
                        nav.segmented aria-label="Чьё использование" {
                            @if everyone {
                                a.active href=(format!("/?days={days}")) aria-current="true" { "Все пользователи" }
                                a href=(format!("/?scope=me&days={days}")) { "Мой аккаунт" }
                            } @else {
                                a href=(format!("/?days={days}")) { "Все пользователи" }
                                a.active href=(format!("/?scope=me&days={days}")) aria-current="true" { "Мой аккаунт" }
                            }
                        }
                    }
                    (views::period_switch(base, stats.days))
                }
            }
            (views::usage(&stats, &UsageView {
                names: &names,
                admin: user.is_admin(),
                show_libraries: true,
                show_keys: true,
                show_users: everyone,
            }))
        },
    )
}
