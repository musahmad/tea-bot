use std::fmt::Display;

use axum::{
    routing::{get, post},
    Router,
};
use tower_http::services::ServeDir;
use dotenv::dotenv;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing_subscriber;

mod contract;
mod preferences;
mod rounds;
mod slack;
mod tea;
mod terms;
mod tv;

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
    pub image_url: Option<String>,
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
pub struct FirestoreConfig {
    /// GCP project id that owns the Firestore database.
    pub project: String,
    /// Firestore database id. Use "(default)" for the default database.
    #[serde(default = "default_firestore_database")]
    pub database: String,
    /// Collection storing one terms-acceptance document per Slack user id.
    #[serde(default = "default_firestore_collection")]
    pub collection: String,
}

fn default_firestore_database() -> String {
    "(default)".to_string()
}

fn default_firestore_collection() -> String {
    "terms_acceptances".to_string()
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
    /// Omit (or leave `project` empty) to disable terms enforcement, e.g. locally.
    #[serde(default)]
    firestore: Option<FirestoreConfig>,
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
    let (tv_tx, _) = tokio::sync::broadcast::channel::<tv::TvEvent>(32);

    let terms = terms::TermsStore::new(config.firestore.clone()).await;

    // Shared so the picker (Slack) and the post-round tea order (tea loop) read
    // and write the same preferences — including the in-memory store in local dev.
    let prefs = std::sync::Arc::new(preferences::PreferenceStore::new(config.firestore.clone()));

    let slack_interface = SlackInterface::new(
        config.slack_bot_token,
        config.slack_channel,
        config.slack_signing_secret,
        command_tx.clone(),
        config.users.clone(),
        tv_tx.clone(),
        terms,
        prefs.clone(),
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
        let mut tea = Tea::new(message_tx, command_rx, contract, config.firestore, prefs);
        async move {
            tea.run().await;
        }
    });

    let tv_routes = Router::new()
        .route("/tv", get(tv::page_handler))
        .route("/tv/events", get(tv::events_handler))
        .with_state(tv_tx);

    let app = Router::new()
        .route("/slack/events", post(slack::handle_slack_event))
        .route("/slack/commands", post(slack::handle_slash_command))
        .route("/slack/interactivity", post(slack::handle_slack_interactivity))
        .with_state(slack_interface)
        .merge(tv_routes)
        .nest_service("/static", ServeDir::new("static"));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:6969").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
