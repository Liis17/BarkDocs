# Gateway

Parent: [[Index]]

## Назначение

Публичная точка входа BarkDocs. Проверяет API-ключ через [[Auth/AuthService|AuthService]], проксирует запросы к [[Engine/DocsEngine|движку]] и подключает [[Web/McpServer|MCP-сервер]]. Своего состояния не хранит.

## Источники

| Файл | Значимый символ | Роль |
|------|-----------------|------|
| `backend/barkdocs-web/src/main.rs` | `authenticate` | Middleware: проверка ключа через `POST /verify` |
| `backend/barkdocs-web/src/main.rs` | `require_admin` | Пропускает только роль `admin` |
| `backend/barkdocs-web/src/main.rs` | `forward` | Передаёт ответ движка клиенту без изменений |
| `backend/barkdocs-web/src/main.rs` | `router` | Таблица маршрутов и порядок middleware |

## Публичные контракты

Адрес по умолчанию `0.0.0.0:8080`. Обязательные переменные: `AUTH_URL`, `ENGINE_URL`.

| Маршрут | Роль | Поведение |
|---------|------|-----------|
| `GET /healthz` | Без ключа | `ok` |
| `GET /v1/libraries/resolve?name=` | user, admin | Поиск библиотеки, ответ движка |
| `GET /v1/docs?library_id=&query=&limit=` | user, admin | Поиск фрагментов, ответ движка |
| `POST /mcp` | user, admin | См. [[Web/McpServer|MCP-сервер]] |
| `POST /v1/libraries` | admin | Добавление библиотеки по `url` или `name` |
| `POST /v1/libraries/sync` | admin | Внеплановая синхронизация библиотеки |

Ошибки: 401 с `WWW-Authenticate: Bearer` — нет или неверный ключ; 403 — не admin; 503 — auth недоступен; 502 — движок недоступен.

## Зависимости и взаимодействия

- Использует [[Auth/AuthService|AuthService]] (`POST /verify`).
- Использует [[Engine/DocsEngine|DocsEngine]] по внутреннему HTTP.
- Использует `reqwest` с таймаутом 180 секунд и `no_proxy()`.

## Ограничения и важные детали

- Заголовок `Authorization` передаётся в auth как есть; роль не кэшируется, проверка выполняется на каждый запрос.
- Таймаут 180 секунд задан потому, что поиск в реестрах при `resolve` синхронный и может занимать десятки секунд. Основание: комментарий в `main.rs`.

## Решения и основания

- Сбой auth закрывает доступ, а не пропускает запрос. Основание: комментарий в `main.rs` — «Ошибка auth-сервиса закрывает доступ».
- Шлюз без состояния. Основание: описание модуля в `main.rs`.
