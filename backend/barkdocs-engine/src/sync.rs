//! Синхронизация источников и ежедневная актуализация.

use std::time::Duration;

use anyhow::bail;

use crate::{
    AppState,
    chunk::{Chunk, chunk_markdown},
    db,
    sources::{self, Doc, Fetched},
};

/// Как часто искать источники, которым пора обновиться. Сами сроки хранятся в БД,
/// поэтому перезапуск сервиса не сдвигает и не теряет обновления.
const SCHEDULER_TICK: Duration = Duration::from_secs(60 * 60);
/// Сбой сети или лимит GitHub скоро пройдут, а отсутствие доков — вряд ли.
const RETRY_AFTER_FAILURE_HOURS: i32 = 1;
const RETRY_WITHOUT_DOCS_HOURS: i32 = 24;
const NO_DOCS: &str = "документация не найдена";

pub async fn scheduler(state: AppState) {
    let mut ticker = tokio::time::interval(SCHEDULER_TICK);
    loop {
        ticker.tick().await;
        match db::due_sources(&state.pool).await {
            Ok(ids) => {
                tracing::info!("плановая актуализация: источников к проверке {}", ids.len());
                for id in ids {
                    spawn_sync(&state, id, false);
                }
            }
            Err(err) => tracing::error!("планировщик: {err}"),
        }
    }
}

pub async fn start_pending(state: &AppState, library_id: i64) -> sqlx::Result<()> {
    for id in db::pending_sources(&state.pool, library_id).await? {
        spawn_sync(state, id, false);
    }
    Ok(())
}

pub fn spawn_sync(state: &AppState, source_id: i64, force: bool) {
    let state = state.clone();
    tokio::spawn(async move {
        let Ok(_slot) = state.sync_slots.acquire().await else {
            return;
        };
        match db::claim_source(&state.pool, source_id, force).await {
            Ok(Some(source)) => sync_source(&state, source_id, source).await,
            Ok(None) => {}
            Err(err) => tracing::error!("источник {source_id}: {err}"),
        }
    });
}

async fn sync_source(state: &AppState, source_id: i64, source: db::ClaimedSource) {
    let result = run(state, source_id, &source).await;
    let saved = match &result {
        Ok(Outcome::Updated(chunks)) => {
            tracing::info!("{}: проиндексировано чанков: {chunks}", source.url);
            Ok(())
        }
        Ok(Outcome::Unchanged) => {
            tracing::info!("{}: без изменений", source.url);
            db::mark_unchanged(&state.pool, source_id).await
        }
        Err(err) => {
            tracing::warn!("{}: {err:#}", source.url);
            let message = format!("{err:#}");
            let retry = if message == NO_DOCS {
                RETRY_WITHOUT_DOCS_HOURS
            } else {
                RETRY_AFTER_FAILURE_HOURS
            };
            db::mark_failed(&state.pool, source_id, &message, retry).await
        }
    };
    if let Err(err) = saved {
        tracing::error!("источник {source_id}: {err}");
    }
}

enum Outcome {
    Unchanged,
    Updated(usize),
}

async fn run(
    state: &AppState,
    source_id: i64,
    source: &db::ClaimedSource,
) -> anyhow::Result<Outcome> {
    let fetched =
        sources::fetch(state, &source.kind, &source.url, source.version.as_deref()).await?;
    let Fetched::Updated { version, docs } = fetched else {
        return Ok(Outcome::Unchanged);
    };
    let chunks = tokio::task::spawn_blocking(move || chunk_docs(docs)).await?;
    // Пустой результат почти всегда сбой или смена вёрстки: старый индекс лучше никакого.
    if chunks.is_empty() {
        bail!(NO_DOCS);
    }
    db::replace_chunks(&state.pool, source_id, &version, &chunks).await?;
    Ok(Outcome::Updated(chunks.len()))
}

fn chunk_docs(docs: Vec<Doc>) -> Vec<(String, Chunk)> {
    docs.iter()
        .flat_map(|doc| {
            chunk_markdown(&doc.markdown)
                .into_iter()
                .map(|chunk| (doc.url.clone(), chunk))
        })
        .collect()
}
