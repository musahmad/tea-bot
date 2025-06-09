use axum::{
    http::StatusCode,
    routing::post,
    Json, Router,
};
use lazy_static::lazy_static;
use rand::Rng;
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing_subscriber;
use dotenv::dotenv;
use std::env;
lazy_static! {
    static ref USERNAME_CACHE: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
    static ref ACTIVE_TEA_OFFER: Mutex<Option<String>> = Mutex::new(None);
    static ref TEA_RESPONSES: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());
}

const TEA_WAIT_TIME_SECONDS: u64 = 30;
const TEA_TIMER_DURATION_MINUTES: u64 = 5;

#[derive(Deserialize, Debug)]
struct SlackEventCallback {
    token: String,
    team_id: String,
    event: SlackEventData,
    #[serde(rename = "type")]
    event_type: String,
}

#[derive(Deserialize, Debug)]
struct SlackEventData {
    #[serde(rename = "type")]
    event_type: String,
    user: String,
    text: String,
    channel: String,
    event_ts: String,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum SlackEvent {
    UrlVerification {
        #[serde(rename = "type")]
        event_type: String,
        challenge: String,
    },
    EventCallback(SlackEventCallback),
}

#[derive(Serialize)]
struct SlackResponse {
    challenge: String,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt::init();
    
    let app = Router::new()
        .route("/slack/events", post(handle_slack_event))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:6969").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}



#[axum::debug_handler]
async fn handle_slack_event(Json(payload): Json<SlackEvent>) -> (StatusCode, Json<SlackResponse>) {
    match payload {
        SlackEvent::UrlVerification { challenge, .. } => {
            (StatusCode::OK, Json(SlackResponse { challenge }))
        }
        SlackEvent::EventCallback(callback) => {
            if let Ok(ts) = callback.event.event_ts.parse::<f64>() {
            
                let current_time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();

                if current_time - ts > 5.0 {
                    return (
                        StatusCode::OK,
                        Json(SlackResponse {
                            challenge: String::new(),
                        }),
                    );
                }
            }

            let username = get_username(&callback.event.user).await.unwrap();

            let _ = message_matcher(&callback.event.text, &username).await;
            (
                StatusCode::OK,
                Json(SlackResponse {
                    challenge: String::new(),
                }),
            )
        }
    }
}

async fn send_slack_message(message: &str) -> Result<(String, String), BoxError> {
    let slack_token = env::var("SLACK_BOT_TOKEN")
        .expect("SLACK_BOT_TOKEN must be set");
        
    let client = reqwest::Client::new();
    let response = client
        .post("https://slack.com/api/chat.postMessage")
        .header(
            "Authorization",
            format!("Bearer {}", slack_token)
        )
        .json(&json!({ "channel": "t", "text": message }))
        .send()
        .await?;
    let response_json: serde_json::Value = response.json().await?;
    let timestamp = response_json["ts"].as_str().unwrap_or("").to_string();
    let channel_id = response_json["channel"].as_str().unwrap_or("").to_string();
    Ok((timestamp, channel_id))
}

async fn update_slack_message(message: &str, timestamp: &str, channel_id: &str) -> Result<(), BoxError> {
    let slack_token = env::var("SLACK_BOT_TOKEN")
        .expect("SLACK_BOT_TOKEN must be set");
    let client = reqwest::Client::new();
    client
        .post("https://slack.com/api/chat.update")
        .header("Authorization", format!("Bearer {}", slack_token))
        .json(&json!({ "channel": channel_id, "text": message, "ts": timestamp.to_string() }))
        .send()
        .await?;
    Ok(())
}

async fn get_username(user_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(cached_name) = USERNAME_CACHE.lock().unwrap().get(user_id) {
        return Ok(cached_name.clone());
    }

    let slack_token = env::var("SLACK_BOT_TOKEN")
        .expect("SLACK_BOT_TOKEN must be set");

    let client = reqwest::Client::new();
    let response = client
        .get("https://slack.com/api/users.info")
        .header(
            "Authorization",
            format!("Bearer {}", slack_token)
        )
        .query(&[("user", user_id)])
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let username = response["user"]["name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();


    USERNAME_CACHE
        .lock()
        .unwrap()
        .insert(user_id.to_string(), username.clone());

    Ok(username)
}

async fn message_matcher(message: &str, username: &str) -> Result<(), BoxError> {
    match message {
        "rt" => request_tea(username).await,
        "ot" => offer_tea(username).await,
        "t" => {
            if username == "alexander.stepanov" {
                send_slack_message("Sorry Sasha, only standard tea is available. Please type 'promise' to promise that you will accept standard tea.").await?;
                return Ok(());
            }
            if let Some(_offerer) = ACTIVE_TEA_OFFER.lock().unwrap().as_ref() {
                TEA_RESPONSES.lock().unwrap().insert(username.to_string());
            }           
            Ok(())
        }
        "c" => cancel_timer().await,
        "promise" => {
            if username == "alexander.stepanov" {
                send_slack_message("Thanks for being reasonable Sasha! I'm glad you saw sense. I'll add you to the tea round.").await?;
                TEA_RESPONSES.lock().unwrap().insert(username.to_string());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn offer_tea(username: &str) -> Result<(), BoxError> {
    let username = username.to_string();
    let result = tokio::spawn(async move {
        {

            let mut active_offer = ACTIVE_TEA_OFFER.lock().unwrap();
            if active_offer.is_some() {
                return Ok::<(), BoxError>(());
            }
            *active_offer = Some(username.clone());

        }

        send_slack_message(&format!("{} has generously offered tea! Type 't' within the next {} seconds to accept", username, TEA_WAIT_TIME_SECONDS)).await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(TEA_WAIT_TIME_SECONDS)).await;
        let responses = {
            let mut responses = TEA_RESPONSES.lock().unwrap().clone().iter().cloned().collect::<Vec<_>>();
            responses.push(username.clone());
            responses
        };
        if responses.len() == 1 {
            send_slack_message(&format!("No one accepted the tea offer 😢. {}, go and treat yourself to a selfish tea. I'll start a 5 minute timer for the perfect brew. Type 'c' to cancel.", username)).await?;
            tea_timer(1).await?;
            return Ok(());
        } else {
            send_slack_message(&format!("This tea round: {}. {} kindly go and make {} cups of tea. I'll start a {} minute timer for the perfect brew. Type 'c' to cancel.", responses.join(", "), username, responses.len(), TEA_TIMER_DURATION_MINUTES)).await?;
            tea_timer(responses.len()).await?;
        }
        Ok(())
    }).await??;

    Ok(result)
}

async fn request_tea(username: &str) -> Result<(), BoxError> {
    let username = username.to_string();
    let result = tokio::spawn(async move {
        {
            let mut active_offer = ACTIVE_TEA_OFFER.lock().unwrap();
            if active_offer.is_some() {
                return Ok::<(), BoxError>(());
            }
            *active_offer = Some(username.clone());
        }
        send_slack_message(&format!("{} is requesting tea! Type 't' within the next {} seconds to join the tea round. You will be entered into a draw to make this round of tea!", username, TEA_WAIT_TIME_SECONDS)).await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(TEA_WAIT_TIME_SECONDS)).await;

        let responses = {
            let mut responses = TEA_RESPONSES.lock().unwrap().clone().iter().cloned().collect::<Vec<_>>();
            responses.push(username.clone());
            responses
        };

        if responses.len() == 1 {
            send_slack_message(&format!("No one accepted the tea request 😢. {}, looks like you'll have to go and treat yourself to a selfish tea. I'll start a {} minute timer for the perfect brew. Type 'c' to cancel.", username, TEA_TIMER_DURATION_MINUTES)).await?;
            tea_timer(1).await?;
            return Ok(());
        } else {
            send_slack_message(&format!(
                "This tea round: {}. Rolling dice... 🎲🎲🎲",
                responses.join(", ")
            ))
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        let message = {
            let mut rng: rand::prelude::ThreadRng = rand::thread_rng();
            let mut rolls: Vec<(String, u32)> =
                    responses.iter().map(|name| (name.clone(), (0..3).map(|_| rng.gen_range(1..=6)).sum())).collect();

            rolls.sort_by(|a, b| b.1.cmp(&a.1));

                let tea_maker = rolls.last().unwrap().0.clone();

                let mut suffix = "";
                if tea_maker == "m" {
                    suffix = "\nha, go make tea baldy";
                }

                format!(
                    "Dice roll results: \n\n{}\n\n{} rolled the lowest number and will make the tea! I'll start a {} minute timer for the perfect brew. Type 'c' to cancel. {}",
                    rolls
                        .iter()
                        .map(|(name, num)| format!("{}: {}", name, num))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    tea_maker,
                    TEA_TIMER_DURATION_MINUTES,
                    suffix
                )
            };

            send_slack_message(&message.to_owned()).await?;
        
        
            tea_timer(responses.len()).await?;
        }
        Ok(())
    }).await??;

    Ok(result)
}

async fn tea_timer(num_tea: usize) -> Result<(), BoxError> {
    let (timestamp, channel_id) = send_slack_message(&format!("Tea timer started: {} minutes left to brew.", TEA_TIMER_DURATION_MINUTES)).await?; 

    let total_seconds = TEA_TIMER_DURATION_MINUTES * 60;
    for seconds_left in (0..total_seconds).rev().step_by(15) {
        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
        
        if ACTIVE_TEA_OFFER.lock().unwrap().is_none() {
            break;
        }

        let minutes = seconds_left / 60;
        let secs = seconds_left % 60;
        update_slack_message(
            &format!(
                "Tea timer started: {}:{:02} left to brew.", 
                minutes,
                secs
            ),
            &timestamp,
            &channel_id
        ).await?;
    }

    if ACTIVE_TEA_OFFER.lock().unwrap().is_some() {
        send_slack_message(&format!("Tea is ready! {}", "☕".repeat(num_tea))).await?;
    }
    {
        let mut responses = TEA_RESPONSES.lock().unwrap();
        responses.clear();
        let mut active_offer = ACTIVE_TEA_OFFER.lock().unwrap();
        *active_offer = None;
    }
    Ok(())
}

async fn cancel_timer() -> Result<(), BoxError> {
    {
        let mut active_offer = ACTIVE_TEA_OFFER.lock().unwrap();
        if active_offer.is_some() {
            *active_offer = None;
        }
        let mut responses = TEA_RESPONSES.lock().unwrap();
        responses.clear();
    }
    send_slack_message(&format!("Tea timer cancelled")).await?;
    Ok(())
}
