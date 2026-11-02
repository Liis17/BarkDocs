//! Вход, регистрация, выход и смена пароля.

use axum::{
    Form,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Redirect, Response},
};
use maud::html;
use serde::Deserialize;

use crate::{
    AppState,
    session::{
        CurrentUser, Flash, FlashKind, PendingFlash, SESSION_COOKIE, clear_session_cookie, cookie,
        redirect_with, session_cookie,
    },
    views::{self, Ctx, Nav},
};

fn error(message: impl Into<String>) -> Option<Flash> {
    Some(Flash {
        kind: FlashKind::Error,
        message: message.into(),
    })
}

fn login_form(flash: Option<Flash>, login: &str, allow_registration: bool) -> Response {
    views::bare_page(
        "Вход",
        flash,
        html! {
            div.card {
                h1 { "Вход" }
                form.stack method="post" action="/login" {
                    label { "Логин" input name="login" value=(login) autocomplete="username" required autofocus; }
                    label { "Пароль" input name="password" type="password" autocomplete="current-password" required; }
                    button.button type="submit" { "Войти" }
                }
                @if allow_registration {
                    p.muted { "Нет аккаунта? " a href="/register" { "Зарегистрироваться" } }
                }
            }
        },
    )
}

pub async fn login_page(
    State(state): State<AppState>,
    PendingFlash(flash): PendingFlash,
) -> Response {
    login_form(flash, "", state.allow_registration)
}

#[derive(Deserialize)]
pub struct LoginForm {
    login: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    match state.services.login(&form.login, &form.password).await {
        Ok(session) => {
            let mut response = Redirect::to("/").into_response();
            response.headers_mut().append(
                header::SET_COOKIE,
                session_cookie(&headers, &state, &session.token),
            );
            response
        }
        Err(err) => login_form(error(err.message), &form.login, state.allow_registration),
    }
}

fn register_form(flash: Option<Flash>, login: &str) -> Response {
    views::bare_page(
        "Регистрация",
        flash,
        html! {
            div.card {
                h1 { "Регистрация" }
                p.muted { "После регистрации вы сможете выпустить ключи MCP для своих агентов и смотреть их использование." }
                form.stack method="post" action="/register" {
                    label { "Логин" input name="login" value=(login) autocomplete="username" required minlength="3" maxlength="64" pattern="[A-Za-z0-9._@\\-]+" autofocus; }
                    label { "Пароль" input name="password" type="password" autocomplete="new-password" required minlength="8"; }
                    label { "Пароль ещё раз" input name="password2" type="password" autocomplete="new-password" required minlength="8"; }
                    button.button type="submit" { "Создать аккаунт" }
                }
                p.muted { "Уже есть аккаунт? " a href="/login" { "Войти" } }
            }
        },
    )
}

pub async fn register_page(State(state): State<AppState>) -> Response {
    if !state.allow_registration {
        return login_form(error("Регистрация отключена администратором."), "", false);
    }
    register_form(None, "")
}

#[derive(Deserialize)]
pub struct RegisterForm {
    login: String,
    password: String,
    password2: String,
}

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Response {
    if form.password != form.password2 {
        return register_form(error("Пароли не совпадают."), &form.login);
    }
    if let Err(err) = state.services.register(&form.login, &form.password).await {
        return register_form(error(err.message), &form.login);
    }
    match state.services.login(&form.login, &form.password).await {
        Ok(session) => {
            let mut response = redirect_with(
                &headers,
                &state,
                "/keys",
                FlashKind::Ok,
                "Аккаунт создан. Выпустите ключ для своего агента.",
            );
            response.headers_mut().append(
                header::SET_COOKIE,
                session_cookie(&headers, &state, &session.token),
            );
            response
        }
        Err(err) => login_form(error(err.message), &form.login, state.allow_registration),
    }
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie(&headers, SESSION_COOKIE).filter(|t| !t.is_empty())
        && let Err(err) = state.services.logout(token).await
    {
        tracing::warn!("выход: {err}");
    }
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, clear_session_cookie(&headers, &state));
    response
}

pub async fn account_page(current: CurrentUser, PendingFlash(flash): PendingFlash) -> Response {
    let user = &current.user;
    views::page(
        Ctx {
            user,
            flash,
            nav: Nav::Account,
        },
        "Аккаунт",
        html! {
            div.page-head { h1 { "Аккаунт" } }
            div.grid-2 {
                section.card {
                    h2 { "Профиль" }
                    dl.props {
                        dt { "Логин" } dd { (user.login) }
                        dt { "Роль" } dd { @if user.is_admin() { "администратор" } @else { "пользователь" } }
                        dt { "Создан" } dd { (views::time(&user.created_at)) " UTC" }
                    }
                }
                section.card {
                    h2 { "Смена пароля" }
                    form.stack method="post" action="/account/password" {
                        label { "Текущий пароль" input name="current" type="password" autocomplete="current-password" required; }
                        label { "Новый пароль" input name="new" type="password" autocomplete="new-password" required minlength="8"; }
                        label { "Новый пароль ещё раз" input name="new2" type="password" autocomplete="new-password" required minlength="8"; }
                        button.button type="submit" { "Сменить пароль" }
                    }
                    p.muted { "Остальные сессии этого аккаунта будут завершены." }
                }
            }
        },
    )
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
    new2: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: CurrentUser,
    Form(form): Form<PasswordForm>,
) -> Response {
    let (kind, message) = if form.new != form.new2 {
        (FlashKind::Error, "Новые пароли не совпадают.".to_string())
    } else {
        match state
            .services
            .change_password(&current.token, &form.current, &form.new)
            .await
        {
            Ok(()) => (FlashKind::Ok, "Пароль изменён.".to_string()),
            Err(err) => (FlashKind::Error, err.message),
        }
    };
    redirect_with(&headers, &state, "/account", kind, &message)
}
