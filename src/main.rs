use std::fmt::Display;

use axum::{routing::post, Router};
use dotenv::dotenv;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing_subscriber;

mod contract;
mod slack;
mod tea;

use crate::{
    contract::ContractInterface,
    slack::{SlackAction, SlackInterface, UserCommand},
    tea::Tea,
};

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct User {
    #[serde(rename = "slack_id")]
    pub id: String,
    pub name: String,
    pub address: String,
    pub emoji: Option<String>,
}
impl Eq for User {}
impl PartialEq for User {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl std::hash::Hash for User {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}
impl Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.emoji.as_ref().unwrap_or(&self.name))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct AppConfig {
    slack_bot_token: String,
    slack_channel: String,
    slack_signing_secret: String,
    private_key: String,
    wallet_address: String,
    provider_url: String,
    users: Vec<User>,
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt::init();

    let config: AppConfig = if let Ok(config_json) = std::env::var("CONFIG_JSON") {
        serde_json5::from_str(&config_json)
    } else {
        let config_path =
            std::env::var("CONFIG_PATH").expect("CONFIG_JSON or CONFIG_PATH should be set");
        let mut file = std::fs::File::open(config_path).expect("File should exist");
        serde_json5::from_reader(&mut file)
    }
    .expect("Config should be good!");

    let config_str = serde_json::to_string_pretty(&config).unwrap();
    tracing::info!("Using config:: {}", config_str);

    let (command_tx, command_rx) = mpsc::unbounded_channel::<UserCommand>();
    let (message_tx, message_rx) = mpsc::unbounded_channel::<SlackAction>();

    let slack_interface = SlackInterface::new(
        config.slack_bot_token,
        config.slack_channel,
        config.slack_signing_secret,
        command_tx.clone(),
        config.users.clone(),
    );

    let contract = ContractInterface::new(
        config.private_key,
        config.wallet_address,
        config.provider_url,
        config.users,
    );

    tokio::spawn({
        let slack = slack_interface.clone();
        async move {
            slack.run(message_rx).await;
        }
    });

    tokio::spawn({
        let mut tea = Tea::new(message_tx, command_rx, contract);
        async move {
            tea.run().await;
        }
    });

    let app = Router::new()
        .route("/slack/events", post(slack::handle_slack_event))
        .route("/slack/commands", post(slack::handle_slash_command))
        .with_state(slack_interface);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:6969").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
