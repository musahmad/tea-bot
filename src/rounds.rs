use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{FirestoreConfig, User};

/// Collection that stores one document per completed (or abandoned) tea round.
/// Deliberately separate from the terms collection so the two never collide.
const ROUNDS_COLLECTION: &str = "tea_rounds";

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Seconds since the Unix epoch, or 0 if the clock is before the epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Everything worth remembering about a single tea round. Holds borrowed
/// domain values straight from `end_tea_round`; all the conversion to plaintext
/// names and Firestore's typed-value envelope happens inside this module so the
/// tea loop never has to think about persistence.
pub struct RoundSummary<'a> {
    pub started_at_unix: u64,
    pub ended_at_unix: u64,
    /// "completed" for a normal round, "lonely" when nobody else joined.
    pub status: &'a str,
    pub starter: &'a User,
    pub bids: &'a HashMap<User, u8>,
    /// Teas to be made this round, grouped by tea (largest group first),
    /// mirroring the Slack tea order. Empty for older records.
    pub teas: &'a [(String, Vec<User>)],
    pub lowest_bid: u8,
    /// One entry per re-roll when the lowest bid was tied; empty otherwise.
    pub rolloff: &'a [Vec<(User, Vec<u8>)>],
    pub loser: Option<&'a User>,
    pub penalty_dice: Option<u8>,
    pub penalty_amount: f64,
    /// Signed net position per player; `None` for a lonely round.
    pub payments: Option<&'a HashMap<User, f64>>,
    /// Computed pairwise transfers `((from, to), amount)`.
    pub transfers: &'a [((User, User), f64)],
    /// Actually-settled amounts, parallel to `transfers` (may be capped at the
    /// payer's on-chain balance, so can differ from the computed amount).
    pub settled: &'a [f64],
    /// On-chain transaction hash once the batch settled, if it did.
    pub tx_hash: Option<String>,
    /// Post-round balance snapshot (the Teaderboard); `None` if unavailable.
    pub balances_after: Option<&'a HashMap<User, f64>>,
}

impl<'a> RoundSummary<'a> {
    /// A round that timed out with a single bidder — recorded for posterity,
    /// but there is no loser, penalty, or settlement.
    pub fn lonely(
        started_at_unix: u64,
        ended_at_unix: u64,
        starter: &'a User,
        bids: &'a HashMap<User, u8>,
        teas: &'a [(String, Vec<User>)],
    ) -> Self {
        Self {
            started_at_unix,
            ended_at_unix,
            status: "lonely",
            starter,
            bids,
            teas,
            lowest_bid: bids.values().copied().next().unwrap_or(0),
            rolloff: &[],
            loser: None,
            penalty_dice: None,
            penalty_amount: 0.0,
            payments: None,
            transfers: &[],
            settled: &[],
            tx_hash: None,
            balances_after: None,
        }
    }
}

/// Durable history of tea rounds, backed by Firestore (native mode). Mirrors
/// `TermsStore`: on Cloud Run it authenticates via the instance metadata server,
/// and an empty project id disables persistence for local dev.
pub struct RoundStore {
    client: Client,
    firestore: Option<FirestoreConfig>,
}

impl RoundStore {
    pub fn new(firestore: Option<FirestoreConfig>) -> Self {
        // An empty project id means "not configured" — same convention as TermsStore.
        let firestore = firestore.filter(|f| !f.project.trim().is_empty());

        match firestore.as_ref() {
            None => tracing::warn!(
                "RoundStore: no firestore config. Round history DISABLED — rounds will not be saved."
            ),
            Some(f) => tracing::info!(
                "RoundStore: recording rounds to firestore {}/{}/{}",
                f.project,
                f.database,
                ROUNDS_COLLECTION
            ),
        }

        Self {
            client: Client::new(),
            firestore,
        }
    }

    async fn access_token(&self) -> Option<String> {
        self.client
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| tracing::error!("RoundStore: failed to fetch access token: {}", e))
            .ok()?
            .json::<TokenResponse>()
            .await
            .map_err(|e| tracing::error!("RoundStore: failed to parse token response: {}", e))
            .ok()
            .map(|t| t.access_token)
    }

    /// Persist one round. Best-effort and non-fatal: failures are logged, never
    /// propagated, so a Firestore hiccup can't disrupt the game. A no-op when
    /// persistence is disabled.
    pub async fn record(&self, summary: RoundSummary<'_>) {
        let Some(firestore) = self.firestore.as_ref() else {
            return;
        };

        let Some(token) = self.access_token().await else {
            tracing::error!("RoundStore: no access token; round not recorded");
            return;
        };

        // Firestore assigns the document id when we POST to the collection.
        let url = format!(
            "https://firestore.googleapis.com/v1/projects/{}/databases/{}/documents/{}",
            firestore.project, firestore.database, ROUNDS_COLLECTION
        );

        let doc = build_document(&summary);

        match self
            .client
            .post(url)
            .bearer_auth(token)
            .json(&doc)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!("RoundStore: recorded {} round", summary.status);
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                tracing::error!("RoundStore: write failed ({}): {}", status, body);
            }
            Err(e) => tracing::error!("RoundStore: write error: {}", e),
        }
    }
}

// --- Firestore typed-value helpers -----------------------------------------
// The REST API wants every scalar wrapped in a `{ "<type>Value": ... }` object,
// and containers nested as arrayValue / mapValue. These keep `build_document`
// readable.

fn sv(v: impl Into<String>) -> Value {
    json!({ "stringValue": v.into() })
}
fn iv(v: i64) -> Value {
    json!({ "integerValue": v.to_string() })
}
fn dv(v: f64) -> Value {
    json!({ "doubleValue": v })
}
fn av(values: Vec<Value>) -> Value {
    json!({ "arrayValue": { "values": values } })
}
fn mv(fields: Value) -> Value {
    json!({ "mapValue": { "fields": fields } })
}
fn nullv() -> Value {
    json!({ "nullValue": null })
}

fn build_document(s: &RoundSummary<'_>) -> Value {
    let players = s.bids.len();

    let bids: Vec<Value> = s
        .bids
        .iter()
        .map(|(user, bid)| {
            mv(json!({
                "player": sv(user.name.clone()),
                "bid": iv(*bid as i64),
            }))
        })
        .collect();

    let rolloff: Vec<Value> = s
        .rolloff
        .iter()
        .enumerate()
        .map(|(round, rolls)| {
            let roll_values: Vec<Value> = rolls
                .iter()
                .map(|(user, dice)| {
                    let sum: u32 = dice.iter().map(|d| *d as u32).sum();
                    mv(json!({
                        "player": sv(user.name.clone()),
                        "dice": av(dice.iter().map(|d| iv(*d as i64)).collect()),
                        "sum": iv(sum as i64),
                    }))
                })
                .collect();
            mv(json!({
                "round": iv((round + 1) as i64),
                "rolls": av(roll_values),
            }))
        })
        .collect();

    let teas: Vec<Value> = s
        .teas
        .iter()
        .map(|(tea, players)| {
            mv(json!({
                "tea": sv(tea.clone()),
                "count": iv(players.len() as i64),
                "players": av(players.iter().map(|u| sv(u.name.clone())).collect()),
            }))
        })
        .collect();

    let penalty = match s.penalty_dice {
        Some(dice) => mv(json!({
            "dice": iv(dice as i64),
            "multiplier": dv(0.5 * players.saturating_sub(1) as f64),
            "amount": dv(s.penalty_amount),
        })),
        None => nullv(),
    };

    let payments: Vec<Value> = s
        .payments
        .map(|p| {
            p.iter()
                .map(|(user, amount)| {
                    mv(json!({
                        "player": sv(user.name.clone()),
                        "net_amount": dv(*amount),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let transfers: Vec<Value> = s
        .transfers
        .iter()
        .enumerate()
        .map(|(k, ((from, to), computed))| {
            let settled = s.settled.get(k).copied().unwrap_or(*computed);
            mv(json!({
                "from": sv(from.name.clone()),
                "to": sv(to.name.clone()),
                "computed_amount": dv(*computed),
                "settled_amount": dv(settled),
            }))
        })
        .collect();

    let balances_after: Vec<Value> = s
        .balances_after
        .map(|b| {
            b.iter()
                .map(|(user, balance)| {
                    mv(json!({
                        "player": sv(user.name.clone()),
                        "balance": dv(*balance),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let transfers_status = if s.transfers.is_empty() {
        "none"
    } else if s.tx_hash.is_some() {
        "success"
    } else {
        "failed"
    };

    let loser = match s.loser {
        Some(user) => sv(user.name.clone()),
        None => nullv(),
    };

    let tx_hash = match &s.tx_hash {
        Some(hash) => sv(hash.clone()),
        None => nullv(),
    };

    json!({
        "fields": {
            "started_at_unix": iv(s.started_at_unix as i64),
            "ended_at_unix": iv(s.ended_at_unix as i64),
            "status": sv(s.status),
            "starter": sv(s.starter.name.clone()),
            "bids": av(bids),
            "teas": av(teas),
            "lowest_bid": iv(s.lowest_bid as i64),
            "cups": iv(players as i64),
            "tie_rolloff": av(rolloff),
            "loser": loser,
            "penalty": penalty,
            "payments": av(payments),
            "transfers": av(transfers),
            "transfers_status": sv(transfers_status),
            "tx_hash": tx_hash,
            "balances_after": av(balances_after),
        }
    })
}
