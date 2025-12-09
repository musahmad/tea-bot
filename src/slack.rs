use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::User;

#[allow(dead_code)]
pub enum UserCommand {
    Bid(User, f64, Url),
    CancelTeaRound,
}

#[derive(Debug)]
pub enum SlackAction {
    SendMessage(String),
    StartTeaRound(User),
    ConfirmBid(User, f64, Url),
    RejectBid(String, Url),
    RevealBids(Vec<(User, f64)>),
    AnnounceDiceRoll(Vec<User>, f64),
    AnnounceDiceRollTie(Vec<User>),
    RollDice(Vec<(User, Vec<u32>)>),
    AnnounceTeaMaker((User, f64, u32)),
    AnnouncePayments(HashMap<User, f64>),
    AnnounceTransfers(HashMap<(User, User), f64>),
    CancelTeaRound,
    ShowTeaderboard(Vec<(User, f64)>),
}

impl SlackAction {
    pub fn send(self, message_tx: &mpsc::UnboundedSender<SlackAction>) {
        message_tx.send(self).ok();
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "event", rename_all = "snake_case")]
pub enum SlackOuterEvent {
    UrlVerification { challenge: String },
    EventCallback(SlackEvent),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SlackEvent {
    pub r#type: String,
    #[serde(default)]
    pub subtype: Option<String>,
    pub user: Option<String>,
    pub text: Option<String>,
    pub event_ts: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SlashCommandPayload {
    pub token: String,
    pub command: String,
    pub text: String,
    pub user_id: String,
    pub channel_id: String,
    pub response_url: Url,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseType {
    Ephemeral,
}

#[derive(Debug, Deserialize)]
pub struct SlackMessageResponse {
    pub ts: String,
    pub channel: String,
}

#[derive(Debug, Serialize)]
pub struct SlashCommandResponse {
    pub response_type: ResponseType,
    pub text: String,
}

pub struct SlackInterface {
    pub token: String,
    pub channel: String,
    pub signing_secret: String,
    pub client: Client,
    pub command_tx: mpsc::UnboundedSender<UserCommand>,
    pub users: Vec<User>,
}

impl SlackInterface {
    pub fn new(
        token: String,
        channel: String,
        signing_secret: String,
        command_tx: mpsc::UnboundedSender<UserCommand>,
        users: Vec<User>,
    ) -> Arc<Self> {
        Arc::new(Self {
            token,
            channel,
            signing_secret,
            client: reqwest::Client::new(),
            command_tx,
            users,
        })
    }

    pub async fn run(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<SlackAction>) {
        tracing::info!("Started message processing task");

        while let Some(action) = rx.recv().await {
            match action {
                SlackAction::SendMessage(message) => {
                    self.send_message(&message).await;
                }
                SlackAction::StartTeaRound(user) => {
                    self.send_message(&format!(
                        "\n\n☕️ *{} is starting a tea round. Place your bid! You have 45 seconds.*\n\n",
                        user
                    ))
                    .await;
                }
                SlackAction::ConfirmBid(user, bid, response_url) => {
                    self.respond_to_slash_command(
                        &format!("Your bid of {} has been accepted!", bid),
                        &response_url,
                    )
                    .await;
                    self.send_message(&format!("{} has joined the tea round ☕️", user))
                        .await;
                }
                SlackAction::RejectBid(reason, response_url) => {
                    self.respond_to_slash_command(
                        &format!("Your bid has been rejected: {}\n\n", reason),
                        &response_url,
                    )
                    .await;
                }
                SlackAction::RevealBids(bids) => {
                    let mut sorted_bids: Vec<_> = bids.iter().collect();
                    sorted_bids.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

                    let mut message = String::from("\n\n☕️ *Times up! Revealing bids:*\n\n...");

                    if let Some(response) = self.send_message(&message).await {
                        for (index, (user, bid)) in sorted_bids.iter().enumerate() {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            if index < sorted_bids.len() - 1 {
                                message = message
                                    .replace("...", format!("{} bid: {}\n...", user, bid).as_str());
                            } else {
                                message = message
                                    .replace("...", format!("{} bid: {}\n", user, bid).as_str());
                            }
                            self.update_message(&message, &response).await;
                        }
                    }
                }
                SlackAction::AnnounceDiceRoll(users, total) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    self.send_message(&format!(
                       "\n\n☕️ *{} both bid {}! Going back to the old school way to settle this... 🎲 🎲 🎲*\n",
                        users
                            .iter()
                            .map(|user| user.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        total
                    ))
                    .await;
                }
                SlackAction::AnnounceDiceRollTie(users) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    self.send_message(&format!(
                        "\n\n☕️ *Tie between: {}! Rolling again...*\n\n",
                        users
                            .iter()
                            .map(|user| user.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ))
                    .await;
                }
                SlackAction::RollDice(rolls) => {
                    let mut message = String::from("\n\n");
                    if let Some(response) = self.send_message(&message).await {
                        tokio::time::sleep(Duration::from_secs(2)).await;

                        for (user, rolls) in rolls {
                            message += &format!("{}: :dice-rolling:", user);
                            self.update_message(&message, &response).await;

                            for (index, roll) in rolls.iter().enumerate() {
                                tokio::time::sleep(Duration::from_secs(2)).await;

                                if index < rolls.len() - 1 {
                                    message = message.replace(
                                        ":dice-rolling:",
                                        format!(":dice-{}: :dice-rolling:", roll).as_str(),
                                    );
                                } else {
                                    message = message.replace(
                                        ":dice-rolling:",
                                        format!(
                                            ":dice-{}: = {}\n\n",
                                            roll,
                                            rolls.iter().sum::<u32>()
                                        )
                                        .as_str(),
                                    );
                                }
                                self.update_message(&message, &response).await;
                            }
                        }
                    }
                }
                SlackAction::AnnounceTeaMaker((user, bid, num_tea)) => {
                    self.send_message(&format!(
                        "\n\n☕️ *{} will make the tea with a bid of {}! Please go and make {} cups of tea!*\n\n",
                        user, bid, num_tea
                    ))
                    .await;
                }
                SlackAction::AnnouncePayments(payments) => {
                    let mut sorted_payments: Vec<_> = payments.iter().collect();
                    sorted_payments.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

                    tokio::time::sleep(Duration::from_secs(2)).await;

                    let mut message = String::from("\n☕️ *Payments to be made:*\n\n");
                    for (user, amount) in sorted_payments {
                        let emoji = if *amount > 0.0 { "🤑" } else { "☹️" };
                        message += &format!("{} {}: {} TEA\n", emoji, user, amount);
                    }
                    self.send_message(&message).await;
                }
                SlackAction::AnnounceTransfers(transfers) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    self.send_message(&format!(
                        "\n☕️ *Transfers to be made:*\n\n{}",
                        transfers
                            .iter()
                            .map(|((from, to), amount)| format!(
                                "{} → {}: {} TEA",
                                from, to, amount
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ))
                    .await;
                }
                SlackAction::CancelTeaRound => {
                    self.send_message(&format!("Tea round cancelled")).await;
                }
                SlackAction::ShowTeaderboard(balances) => {
                    let mut leaderboard = String::from("\n\n☕️ *Teaderboard*\n\n");
                    let mut sorted_balances: Vec<_> = balances.iter().collect();
                    sorted_balances.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

                    for (i, (user, balance)) in sorted_balances.iter().enumerate() {
                        let medal = match i {
                            0 => "🥇",
                            1 => "🥈",
                            2 => "🥉",
                            _ => "  ",
                        };

                        leaderboard.push_str(&format!(
                            "{} *{}* Balance: {} TEA\n\n",
                            medal, user, balance,
                        ));
                    }
                    self.send_message(&leaderboard).await;
                }
            }
        }

        tracing::info!("Message processing task ended");
    }

    async fn send_message(&self, message: &str) -> Option<SlackMessageResponse> {
        let response = self
            .client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&json!({ "channel": &self.channel, "text": message }))
            .send()
            .await
            .map_err(|e| tracing::error!("Failed to send message: {}", e))
            .ok()?
            .json::<SlackMessageResponse>()
            .await
            .map_err(|e| tracing::error!("Failed to parse message response: {}", e))
            .ok()?;
        Some(response)
    }
    async fn update_message(&self, message: &str, previous_message: &SlackMessageResponse) {
        self
            .client
            .post("https://slack.com/api/chat.update")
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&json!({ "channel": previous_message.channel, "text": message, "ts": previous_message.ts }))
            .send()
            .await
            .map_err(|e| tracing::error!("Failed to update message: {}", e))
            .ok();
    }

    async fn respond_to_slash_command(&self, message: &str, response_url: &Url) {
        self.client
            .post(response_url.as_str())
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&json!({ "response_type": "ephemeral", "text": message }))
            .send()
            .await
            .map_err(|e| tracing::error!("Failed to respond to slash command: {}", e))
            .ok();
    }

    fn get_user(&self, user_id: &str) -> Option<User> {
        self.users.iter().find(|user| user.id == user_id).cloned()
    }

    async fn handle_event(&self, event: SlackOuterEvent) -> (StatusCode, Json<Value>) {
        match event {
            SlackOuterEvent::UrlVerification { challenge } => {
                (StatusCode::OK, Json(json!({ "challenge": challenge })))
            }
            SlackOuterEvent::EventCallback(event) => {
                if event.user.is_none() || event.text.is_none() {
                    return (StatusCode::OK, Json(json!({ "text": "Skipped event" })));
                }

                if event.subtype.is_some() {
                    return (
                        StatusCode::OK,
                        Json(json!({ "text": "Skipped subtype event" })),
                    );
                }

                if let Some(text) = event.text {
                    match text.trim().to_lowercase().as_str() {
                        "c" => {
                            self.command_tx.send(UserCommand::CancelTeaRound).ok();
                        }
                        _ => (),
                    };
                }

                (StatusCode::OK, Json(json!({ "text": "Command received" })))
            }
        }
    }

    async fn handle_slash_command(&self, payload: SlashCommandPayload) -> Response {
        if payload.text.trim().is_empty() {
            return StatusCode::OK.into_response();
        }
        let user = match self.get_user(&payload.user_id) {
            Some(user) => user,
            None => {
                return (
                    StatusCode::OK,
                    Json(SlashCommandResponse {
                        response_type: ResponseType::Ephemeral,
                        text: "You have not been added to the tea bot yet. Please contact the Tea admin to be added!".to_string(),
                    }),
                )
                    .into_response()
            }
        };

        if let Ok(bid) = payload.text.trim().parse::<f64>() {
            if bid < 0.0 {
                return (
                    StatusCode::OK,
                    Json(SlashCommandResponse {
                        response_type: ResponseType::Ephemeral,
                        text: "Invalid bid. Bid must be a non-negative number.".to_string(),
                    }),
                )
                    .into_response();
            }

            if (bid * 10.0).round() != bid * 10.0 {
                return (
                    StatusCode::OK,
                    Json(SlashCommandResponse {
                        response_type: ResponseType::Ephemeral,
                        text: "Invalid bid. Bid must have at most one decimal place (e.g., 5.5)."
                            .to_string(),
                    }),
                )
                    .into_response();
            }

            self.command_tx
                .send(UserCommand::Bid(user, bid, payload.response_url))
                .ok();
            StatusCode::OK.into_response()
        } else {
            (
                StatusCode::OK,
                Json(SlashCommandResponse {
                    response_type: ResponseType::Ephemeral,
                    text: "Invalid bid. Please provide a number.".to_string(),
                }),
            )
                .into_response()
        }
    }
}

fn verify_slack_signature(
    signing_secret: &str,
    timestamp: &str,
    body: &str,
    slack_signature: &str,
) -> Result<(), String> {
    let request_timestamp = timestamp
        .parse::<i64>()
        .map_err(|_| "Invalid timestamp format".to_string())?;

    let current_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "System time error".to_string())?
        .as_secs() as i64;

    let time_diff = (current_timestamp - request_timestamp).abs();
    if time_diff > 5 {
        return Err(format!(
            "Request timestamp is too old. Difference: {} seconds",
            time_diff
        ));
    }

    let sig_basestring = format!("v0:{}:{}", timestamp, body);

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())
        .map_err(|_| "Invalid signing secret".to_string())?;

    mac.update(sig_basestring.as_bytes());

    let result = mac.finalize();
    let code_bytes = result.into_bytes();

    let my_signature = format!("v0={}", hex::encode(code_bytes));

    if constant_time_compare(&my_signature, slack_signature) {
        Ok(())
    } else {
        Err(format!(
            "Signature mismatch. Expected: {}, Got: {}",
            my_signature, slack_signature
        ))
    }
}

fn constant_time_compare(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut result = 0u8;
    for (byte_a, byte_b) in a.bytes().zip(b.bytes()) {
        result |= byte_a ^ byte_b;
    }

    result == 0
}

fn verify_request_signature(
    headers: &HeaderMap,
    signing_secret: &str,
    body: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let timestamp = headers
        .get("x-slack-request-timestamp")
        .ok_or_else(|| {
            tracing::error!("Missing x-slack-request-timestamp header");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing timestamp" })),
            )
        })?
        .to_str()
        .map_err(|_| {
            tracing::error!("Invalid timestamp header");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid timestamp" })),
            )
        })?;

    let slack_signature = headers
        .get("x-slack-signature")
        .ok_or_else(|| {
            tracing::error!("Missing x-slack-signature header");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing signature" })),
            )
        })?
        .to_str()
        .map_err(|_| {
            tracing::error!("Invalid signature header");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid signature" })),
            )
        })?;

    verify_slack_signature(&signing_secret, timestamp, body, slack_signature).map_err(|e| {
        tracing::error!("Signature verification failed: {}", e);
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid signature" })),
        )
    })?;

    Ok(())
}

#[axum::debug_handler]
pub async fn handle_slack_event(
    State(slack): State<Arc<SlackInterface>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Verify signature
    if let Err(error_response) =
        verify_request_signature(&headers, slack.signing_secret.as_str(), &body)
    {
        return error_response;
    }

    match serde_json::from_str::<SlackOuterEvent>(&body) {
        Ok(event) => slack.handle_event(event).await,
        Err(e) => {
            tracing::error!("Failed to parse as SlackOuterEvent: {} Body: {}", e, body);
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid JSON" })),
            )
        }
    }
}

#[axum::debug_handler]
pub async fn handle_slash_command(
    State(slack): State<Arc<SlackInterface>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(error_response) =
        verify_request_signature(&headers, slack.signing_secret.as_str(), &body)
    {
        return (
            error_response.0,
            Json(SlashCommandResponse {
                response_type: ResponseType::Ephemeral,
                text: "Invalid signature".to_string(),
            }),
        )
            .into_response();
    }

    match serde_urlencoded::from_str::<SlashCommandPayload>(&body) {
        Ok(payload) => slack.handle_slash_command(payload).await,
        Err(e) => {
            tracing::error!("Failed to parse slash command payload: {}", e);
            (
                StatusCode::OK,
                Json(SlashCommandResponse {
                    response_type: ResponseType::Ephemeral,
                    text: "Invalid payload".to_string(),
                }),
            )
                .into_response()
        }
    }
}
