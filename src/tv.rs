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

use crate::User;

#[derive(Clone, Debug, Serialize)]
pub struct TvUser {
    pub name: String,
    pub image_url: Option<String>,
}

impl TvUser {
    pub fn from_user(user: &User) -> Self {
        TvUser {
            name: user.name.clone(),
            image_url: user.image_url.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
pub enum TvEvent {
    TeaRoundStarted { started_by: TvUser },
    TeaRoundCancelled,
    BidRevealed { user: TvUser, bid: u8 },
    DiceRollAnnounced { rollers: Vec<TvUser>, tied_bid: u8 },
    DiceRolling { user: TvUser },
    DiceResult { user: TvUser, die_index: u8, value: u8 },
    DiceRollTie { rollers: Vec<TvUser> },
    TeaMakerAnnounced { maker: TvUser, bid: u8, cups: usize },
    PenaltyRolling,
    PenaltyRevealed { value: u8 },
    PaymentsAnnounced { payments: Vec<(TvUser, f64)> },
    Teaderboard { entries: Vec<(TvUser, f64)> },
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
