//! `/events` list, acknowledge, and the SSE stream.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use futures_util::{stream, Stream};
use ss_core::{Event, EventId, EventQuery};
use tokio::sync::broadcast::error::RecvError;

use crate::auth::Viewer;
use crate::error::{ApiQuery, ApiResult};
use crate::stream::bounded;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/events", get(list_events))
        .route("/events/stream", get(stream_events))
        .route("/events/{id}/ack", post(ack_event))
}

async fn list_events(
    State(st): State<AppState>,
    _v: Viewer,
    ApiQuery(q): ApiQuery<EventQuery>,
) -> ApiResult<Json<Vec<Event>>> {
    Ok(Json(st.store.list_events(&q)?))
}

async fn ack_event(
    State(st): State<AppState>,
    _v: Viewer,
    Path(id): Path<EventId>,
) -> ApiResult<Json<Event>> {
    Ok(Json(st.store.acknowledge_event(id)?))
}

/// Server-sent events: `event: <kind>`, `data: <Event JSON>`.
async fn stream_events(
    State(st): State<AppState>,
    Viewer(me): Viewer,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = st.engine.subscribe_events();
    let events = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => match SseEvent::default().event(ev.kind.as_str()).json_data(&ev) {
                    Ok(sse) => return Some((Ok::<SseEvent, Infallible>(sse), rx)),
                    Err(e) => tracing::warn!(error = %e, "could not encode event for SSE"),
                },
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "SSE subscriber lagged");
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(bounded(&st, me.token, events)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
