# Architecture

Parent: [[Index]]

## Назначение и границы

BarkDocs отвечает агентам на вопрос «какая документация у библиотеки и что в ней сказано». Агенты обращаются к `barkdocs-web` (порт 8080), люди — к веб-консоли `barkdocs-admin` (порт 8083). `barkdocs-auth` публикуется на `127.0.0.1:8081` для управления ключами мастер-ключом, `barkdocs-engine` наружу не публикуется.
Документация, которую движок скачивает из интернета, и адреса из реестров считаются недоверенными: см. [[Engine/SafeHttp|Безопасный HTTP-клиент]].

## Стек

| Технология | Роль | Источник |
|------------|------|----------|
| Rust, edition 2024 | Язык всех сервисов | `backend/Cargo.toml`, `backend/barkdocs-*/Cargo.toml` |
| axum 0.8 | HTTP-серверы, роутинг, middleware | `backend/Cargo.toml` |
| tokio | Асинхронная среда, фоновый планировщик | `backend/Cargo.toml` |
| sqlx 0.9 (postgres, migrate) | Доступ к БД, миграции при старте | `backend/Cargo.toml`, `backend/*/migrations/` |
| PostgreSQL 17 | Ключи, библиотеки, чанки, полнотекстовый поиск (`tsvector`) | `backend/docker-compose.yml`, `backend/barkdocs-engine/migrations/0001_init.sql` |
| reqwest 0.13 (rustls) | HTTP-клиент: реестры, GitHub, сайты, вызовы между сервисами | `backend/barkdocs-engine/Cargo.toml`, `backend/barkdocs-web/Cargo.toml` |
| scraper, htmd, texting_robots | Разбор HTML, конвертация в markdown, `robots.txt` | `backend/barkdocs-engine/src/sources/site.rs` |
| tar, flate2 | Потоковое чтение tarball GitHub | `backend/barkdocs-engine/src/sources/github.rs` |
| sha2, hex, getrandom | Хеши ключей, сессий и версий, генерация токенов | `backend/barkdocs-auth/src/crypto.rs`, `backend/barkdocs-engine/src/sources/mod.rs` |
| argon2 | Хеши паролей аккаунтов консоли | `backend/barkdocs-auth/src/crypto.rs` |
| maud | HTML-шаблоны веб-консоли | `backend/barkdocs-admin/src/views.rs` |
| Docker Compose, общий Dockerfile | Развёртывание | `backend/docker-compose.yml`, `backend/Dockerfile` |

## Компоненты

- [[Auth/AuthService|Сервис авторизации]] — аккаунты, сессии консоли, API-ключи и роли (`admin` / `user`); единственное место, где хранятся ключи и пароли.
- [[Web/Gateway|Публичный шлюз]] — точка входа агентов; проверяет ключ и проксирует запросы к движку, передавая владельца ключа.
- [[Admin/AdminConsole|Веб-консоль]] — интерфейс для людей: ключи MCP, статистика, библиотеки, пользователи.
- [[Web/McpServer|MCP-сервер]] — MCP поверх того же шлюза, два инструмента.
- [[Engine/DocsEngine|Движок документации]] — хранение, поиск, управление библиотеками, планировщик и синхронизация.
- [[Engine/Usage|Учёт запросов]] — журнал запросов агентов и статистика.
- [[Engine/Discovery|Поиск библиотек]] — выбор кандидатов в реестрах и регистрация источников.
- [[Engine/Sources|Источники документации]] — GitHub, llms.txt, сайты.
- [[Engine/SafeHttp|Безопасный HTTP-клиент]] — общий клиент для всех недоверенных URL.

## Основные потоки

### Запрос клиента к документации

1. Клиент (HTTP или MCP) отправляет запрос на `barkdocs-web` с заголовком `Authorization: Bearer <ключ>`.
2. Middleware `authenticate` пересылает заголовок в `barkdocs-auth` (`POST /verify`). Ответ 401 закрывает доступ; недоступность auth даёт 503. Без проверки запрос не проходит.
3. Роль, владелец и id ключа кладутся в request extensions; маршруты `/v1/libraries` (POST) и `/v1/libraries/sync` проверяют роль `admin`.
4. Шлюз или MCP-сервер вызывает движок (`GET /libraries/resolve`, `GET /docs`) с заголовками владельца, ключа и канала. HTTP-ответ движка передаётся как есть, MCP форматирует его в текст.
5. Движок записывает запрос в журнал [[Engine/Usage|учёта]] в фоне.

Источники: `backend/barkdocs-web/src/main.rs`, `backend/barkdocs-web/src/mcp.rs`, `backend/barkdocs-auth/src/keys.rs`, `backend/barkdocs-engine/src/api.rs`, `backend/barkdocs-engine/src/usage.rs`.

### Работа в веб-консоли

1. Пользователь регистрируется или входит: консоль получает токен сессии от auth (`POST /sessions`) и хранит его в cookie.
2. На каждый запрос консоль получает пользователя и роль через `GET /me` auth.
3. Ключи и пользователи запрашиваются у auth с токеном сессии: auth сам ограничивает пользователя его ключами.
4. Статистика и библиотеки запрашиваются у движка; фильтр по пользователю и доступ к разделам администратора задаёт консоль.

Источники: `backend/barkdocs-admin/src/session.rs`, `backend/barkdocs-admin/src/pages/`, `backend/barkdocs-auth/src/users.rs`.

### Регистрация библиотеки

1. Запрос по имени, для которого нет точного совпадения, или `POST /v1/libraries` (admin) с `url` или `name`. Из консоли — то же автоматическое добавление или ручное (`POST /libraries/custom`) с явным списком источников без поиска в реестрах.
2. [[Engine/Discovery|Поиск библиотек]] опрашивает npm, crates.io и PyPI, выбирает до трёх кандидатов и создаёт библиотеку с источниками `github` и `llms` или `site`.
3. Первичная синхронизация стартует сразу; статус библиотеки `indexing` до появления чанков.

Источники: `backend/barkdocs-engine/src/discovery.rs`, `backend/barkdocs-engine/src/api.rs`, `backend/barkdocs-engine/src/db.rs`.

### Синхронизация источника

1. Планировщик раз в час берёт источники с `next_check_at <= now()`. Ручной запуск — `POST /sync`.
2. `claim_source` атомарно ставит метку `sync_started_at`; метка старше часа считается следом упавшего процесса. Одновременно работает не более четырёх источников.
3. `sources::fetch` возвращает версию (SHA коммита у GitHub, хеш содержимого у llms и сайтов). Совпадение с сохранённой версией означает `Unchanged`.
4. Документы режутся на чанки и заменяются в одной транзакции (`replace_chunks`).
5. Ошибка переносит проверку на 1 час; «документация не найдена» — на 24 часа.

Источники: `backend/barkdocs-engine/src/sync.rs`, `backend/barkdocs-engine/src/db.rs`, `backend/barkdocs-engine/src/sources/mod.rs`.

## Данные

Каждый сервис работает в своей схеме PostgreSQL: `auth` (`users`, `sessions`, `api_keys`) и `engine` (`libraries`, `sources`, `chunks`, `requests`). Консоль своей схемы не имеет. Схема создаётся при старте, миграции `sqlx::migrate!()` применяются тоже при старте.

## Развёртывание

- `docker-compose.yml`: `postgres` (17-alpine, volume `postgres-data`), `barkdocs-auth` (`127.0.0.1:8081`), `barkdocs-engine` (без публикации портов), `barkdocs-web` (`8080`), `barkdocs-admin` (`8083`). Все сервисы собираются из одного `backend/Dockerfile` с build-arg `SERVICE`.
- `Dockerfile`: сборка в `rust:1-slim-trixie`, запуск в `debian:trixie-slim` от пользователя 65534.
- Переменные: `DATABASE_URL`, `ADMIN_KEY`, `POSTGRES_PASSWORD`, `GITHUB_TOKEN` (необязательно), `AUTH_URL`, `ENGINE_URL`, `BIND_ADDR`; для консоли и аккаунтов — `ADMIN_LOGIN`, `ADMIN_PASSWORD`, `ALLOW_REGISTRATION`, `MCP_PUBLIC_URL`, `COOKIE_SECURE`. Значения по умолчанию адресов: `0.0.0.0:8080`, `0.0.0.0:8081`, `0.0.0.0:8082`, `0.0.0.0:8083`.
- Секреты задаются в `backend/.env` по образцу `backend/.env.example`; файл игнорируется git.

## Ограничения и инварианты

- Наружу открыты `barkdocs-web` и `barkdocs-admin`. Движок не проверяет ключи и права: он доверяет шлюзу и консоли, в том числе заголовкам владельца запроса.
- Ключи и сессии хранятся в БД только как SHA-256, пароли — как argon2id. Ключ показывается один раз при создании. Мастер-ключ задаётся через `ADMIN_KEY` (не короче 32 символов), в БД не сохраняется.
- Ключи агентов всегда имеют роль `user`; роль `admin` на шлюзе даёт только мастер-ключ.
- Лимитов на число запросов нет; журнал запросов служит только для статистики.
- Сбой auth закрывает доступ (503), а не пропускает запрос.
- Запросы по недоверенным URL проходят через [[Engine/SafeHttp|Безопасный HTTP-клиент]].
- Пустой результат загрузки не заменяет существующий индекс.
- Замена чанков атомарна: поиск не видит частичный индекс.
- Новый тип источника требует правок в `sources::fetch` и в `CHECK` на `sources.kind` в миграции движка.

## Решения и основания

- **Отдельная схема PostgreSQL на сервис.** Основание: комментарий в `main.rs` каждого сервиса — иначе миграции конфликтуют в общей таблице `_sqlx_migrations`.
- **Авторизация только на шлюзе.** Основание: `backend/barkdocs-engine/src/api.rs` — «Авторизацию выполняет barkdocs-web»; `barkdocs-web` «своего состояния не хранит».
- **Ошибка auth закрывает доступ.** Основание: комментарий в `backend/barkdocs-web/src/main.rs` — «Ошибка auth-сервиса закрывает доступ».
- **Версия источника — хеш содержимого для сайтов и llms.txt.** Основание: комментарий в `backend/barkdocs-engine/src/sources/mod.rs` — сайты и llms.txt не дают надёжного признака изменений.
- **Один Dockerfile и одинаковый дистрибутив в сборке и рантайме.** Основание: комментарий в `backend/Dockerfile` — иначе бинарник не найдёт нужную версию glibc.
- **Консоль без своего хранилища.** Аккаунты и ключи остаются в auth, статистика — рядом с библиотеками в движке; права на ключи auth проверяет сам по токену сессии.
- **Почему выбраны Rust, PostgreSQL FTS и разделение на сервисы.** Не задокументировано: в репозитории нет ADR или пояснений; подтверждены только перечисленные выше основания.

## Добавление компонента

- Новый crate: добавить в `members` `backend/Cargo.toml`, зависимости вынести в `[workspace.dependencies]`, если они общие.
- Новый сервис: строка в `backend/docker-compose.yml` с `build.args.SERVICE`. Схема и миграции — по образцу `main.rs` существующих сервисов.
- Новый источник документации: вариант в `sources::fetch`, модуль в `sources/`, значение в `CHECK` миграции.
