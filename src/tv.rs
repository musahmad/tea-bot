use std::convert::Infallible;

use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html,
    },
};
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum TvEvent {
    TeaRoundStarted { started_by: String },
    TeaRoundCancelled,
    TeaMakerAnnounced { maker: String, bid: u8, cups: usize },
    Teaderboard { entries: Vec<(String, f64)> },
}

pub async fn events_handler(
    State(tx): State<broadcast::Sender<TvEvent>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(tx.subscribe()).filter_map(|result| {
        result.ok().and_then(|event| {
            let json = serde_json::to_string(&event).ok()?;
            Some(Ok::<_, Infallible>(Event::default().data(json)))
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn page_handler() -> Html<&'static str> {
    Html(include_str!("tv.html"))
}
