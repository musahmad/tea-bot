use axum::{Json, Router, http::StatusCode, routing::post};
use dotenv::dotenv;
use lazy_static::lazy_static;
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing_subscriber;

#[derive(Serialize, Deserialize, Clone, Default)]
struct UserStats {
    username: String,
    rounds_participated: u32,
    times_requested: u32,
    times_offered: u32,
    times_lost: u32,
    times_king: u32,
    times_bitch: u32,
    teas_made: u32,
}

lazy_static! {
    static ref USERNAME_CACHE: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
    static ref ACTIVE_TEA_OFFER: Mutex<Option<String>> = Mutex::new(None);
    static ref TEA_RESPONSES: Mutex<std::collections::HashSet<String>> =
        Mutex::new(std::collections::HashSet::new());
    static ref USER_STATS: Mutex<HashMap<String, UserStats>> = Mutex::new(HashMap::new());
}

const TEA_WAIT_TIME_SECONDS: u64 = 30;
const TEA_TIMER_DURATION_MINUTES: u64 = 5;

#[derive(Deserialize, Debug)]
struct SlackEventCallback {
    event: SlackEventData,
}

#[derive(Deserialize, Debug)]
struct SlackEventData {
    user: String,
    text: String,
    event_ts: String,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum SlackEvent {
    UrlVerification { challenge: String },
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

    // Load existing stats from file
    load_stats_from_file().await;

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
    let slack_token = env::var("SLACK_BOT_TOKEN").expect("SLACK_BOT_TOKEN must be set");

    let client = reqwest::Client::new();
    let response = client
        .post("https://slack.com/api/chat.postMessage")
        .header("Authorization", format!("Bearer {}", slack_token))
        .json(&json!({ "channel": "t", "text": message }))
        .send()
        .await?;
    let response_json: serde_json::Value = response.json().await?;
    let timestamp = response_json["ts"].as_str().unwrap_or("").to_string();
    let channel_id = response_json["channel"].as_str().unwrap_or("").to_string();
    Ok((timestamp, channel_id))
}

async fn update_slack_message(
    message: &str,
    timestamp: &str,
    channel_id: &str,
) -> Result<(), BoxError> {
    let slack_token = env::var("SLACK_BOT_TOKEN").expect("SLACK_BOT_TOKEN must be set");
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

    let slack_token = env::var("SLACK_BOT_TOKEN").expect("SLACK_BOT_TOKEN must be set");

    let client = reqwest::Client::new();
    let response = client
        .get("https://slack.com/api/users.info")
        .header("Authorization", format!("Bearer {}", slack_token))
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
    match message.trim().to_ascii_lowercase().as_str() {
        "rt" => request_tea(username).await,
        "ot" => offer_tea(username).await,
        "t" => {
            if let Some(_offerer) = ACTIVE_TEA_OFFER.lock().unwrap().as_ref() {
                TEA_RESPONSES.lock().unwrap().insert(username.to_string());
            }
            Ok(())
        }
        "c" => cancel_timer().await,
        "stats" => {
            if let Ok(leaderboard) = generate_leaderboard().await {
                send_slack_message(&leaderboard).await?;
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
        update_request_or_offer_stats(&username, false).await?;

        send_slack_message(&format!("{} has generously offered tea! Type 't' within the next {} seconds to accept", username, TEA_WAIT_TIME_SECONDS)).await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(TEA_WAIT_TIME_SECONDS)).await;
        let responses = {
            let mut responses = TEA_RESPONSES.lock().unwrap().clone().iter().cloned().collect::<Vec<_>>();
            if !responses.contains(&username) {
                responses.push(username.clone());
            }
            responses
        };
        if responses.len() == 1 {
            send_slack_message(&format!("No one accepted the tea offer 😢. {}, go and treat yourself to a lonely tea. I'll start a 5 minute timer for the perfect brew. Type 'c' to cancel.", username)).await?;
            update_participation_stats(&responses, &username, None, None).await?;
            tea_timer(1).await?;
            return Ok(());
        } else {
            send_slack_message(&format!("This tea round: {}. {} I'll start a {} minute timer for the perfect brew. Type 'c' to cancel.", responses.join(", "), get_helper_message(&responses), TEA_TIMER_DURATION_MINUTES)).await?;
            update_participation_stats(&responses, &username, None, None).await?;
            if let Ok(leaderboard) = generate_leaderboard().await {
                send_slack_message(&leaderboard).await?;
            }
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
        
        update_request_or_offer_stats(&username, true).await?;
        send_slack_message(&format!("{} is requesting tea! Type 't' within the next {} seconds to join the tea round. You will be entered into a draw to make this round of tea!", username, TEA_WAIT_TIME_SECONDS)).await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(TEA_WAIT_TIME_SECONDS)).await;

        let responses = {
            let mut responses = TEA_RESPONSES.lock().unwrap().clone().iter().cloned().collect::<Vec<_>>();
            responses.push(username.clone());
            responses
        };

        if responses.len() == 1 {
            send_slack_message(&format!("No one accepted the tea request 😢. {}, looks like you'll have to go and treat yourself to a selfish tea. I'll start a {} minute timer for the perfect brew. Type 'c' to cancel.", username, TEA_TIMER_DURATION_MINUTES)).await?;
            update_participation_stats(&responses, &username, None, None).await?;
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
            let mut current_players = responses.clone();
            let mut all_roll_results = Vec::new();
            let mut round = 1;
            
            let (tea_maker, king_player, bitch_player) = loop {
                let rolls = roll_dice(&current_players).await?;
                
                // Check if anyone rolled a 3 (minimum possible score)
                let bitch_candidates: Vec<String> = rolls
                    .iter()
                    .filter(|(_, score)| *score == 3)
                    .map(|(name, _)| name.clone())
                    .collect();

                if !bitch_candidates.is_empty() {
                    let bitch = &bitch_candidates[0]; // Take the first one if multiple people rolled 3
                    all_roll_results.push(format!("🎯 {} rolled a 3! You are the bitch for the day and must make ALL the teas! 🫖", bitch));
                    
                    // Send the results with text
                    send_slack_message(&all_roll_results.join("\n\n")).await?;
                    
                    // Send just the image
                    send_slack_image("https://media3.giphy.com/media/v1.Y2lkPTc5MGI3NjExdGUwZGN6dDEwYzZ5dTNuNHN6b3FkbHEyanoxdmk1eW1lNWl0enoyeCZlcD12MV9pbnRlcm5hbF9naWZfYnlfaWQmY3Q9Zw/SiBRuDZmgmBHNo0SOY/giphy.gif").await?;
                    
                    break (bitch.clone(), None, Some(bitch.clone()));
                }

                // Check if anyone rolled an 18 (maximum possible score)
                let king_candidates: Vec<String> = rolls
                    .iter()
                    .filter(|(_, score)| *score == 18)
                    .map(|(name, _)| name.clone())
                    .collect();

                if !king_candidates.is_empty() {
                    let king = &king_candidates[0]; // Take the first one if multiple people rolled 18
                    all_roll_results.push(format!("👑 {} rolled an 18! You are the king for the day and are exempt from making tea! 🏆", king));
                    
                    // Send the results with text
                    send_slack_message(&all_roll_results.join("\n\n")).await?;
                    
                    // Send just the image
                    send_slack_image("https://media1.giphy.com/media/v1.Y2lkPTc5MGI3NjExa2pkc2RzZTd2bWc1OXlzejFqaXVqbzgxMnNudmVlem44YWNsemZjYSZlcD12MV9pbnRlcm5hbF9naWZfYnlfaWQmY3Q9Zw/3o7btXkbsV26U95Uly/giphy.gif").await?;

                    break (rolls.last().unwrap().0.clone(), Some(king.clone()), None);
                }

                let lowest_score = rolls.last().unwrap().1;
                let lowest_rollers: Vec<String> = rolls
                    .iter()
                    .filter(|(_, score)| *score == lowest_score)
                    .map(|(name, _)| name.clone())
                    .collect();

                if lowest_rollers.len() == 1 {
                    break (lowest_rollers[0].clone(), None, None);
                } else {
                    all_roll_results.push(format!("Tie between: {}! Rolling again...", lowest_rollers.join(", ")));
                    current_players = lowest_rollers;
                    round += 1;
                    
                    if round > 1 {
                        send_slack_message(&all_roll_results.join("\n\n")).await?;
                        all_roll_results.clear();
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            };

            // Track participation stats  
            update_participation_stats(&responses, &tea_maker, king_player.as_deref(), bitch_player.as_deref()).await?;
            
            format!("{}\n\n*{}* rolled the lowest number and will make the tea! I'll start a {} minute timer for the perfect brew. {} Type 'c' to cancel.", all_roll_results.join("\n\n"), tea_maker, TEA_TIMER_DURATION_MINUTES, get_helper_message(&responses))
        };

            send_slack_message(&message.to_owned()).await?;
            // Show leaderboard after tea round
            if let Ok(leaderboard) = generate_leaderboard().await {
                send_slack_message(&leaderboard).await?;
            }
            tea_timer(responses.len()).await?;
            
        }
        Ok(())
    }).await??;

    Ok(result)
}

async fn roll_dice(current_players: &[String]) -> Result<Vec<(String, u32)>, BoxError> {
    let mut throws: HashMap<String, Vec<u32>> = HashMap::new();
    let mut timestamp = String::new();
    let mut channel_id = String::new();

    for roll in 1..=3 {
        let mut roll_message = String::new();
        for name in current_players {
            let dice_roll = rand::random::<u32>() % 6 + 1;
            throws
                .entry(name.clone())
                .or_insert(Vec::new())
                .push(dice_roll);
            let dice_display = throws
                .get(name)
                .unwrap()
                .iter()
                .map(|x| format!(":dice-{}:", x))
                .collect::<Vec<_>>()
                .join("  ");
            if roll == 3 {
                let sum = throws.get(name).unwrap().iter().sum::<u32>();
                roll_message.push_str(&format!("{}: {} = {}\n\n", name, dice_display, sum));
            } else {
                roll_message.push_str(&format!("{}: {}\n\n", name, dice_display));
            }
        }

        if roll == 1 {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            (timestamp, channel_id) = send_slack_message(&roll_message).await?;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            update_slack_message(&roll_message, &timestamp, &channel_id).await?;
        }
    }
    send_slack_message("\n").await?;

    let mut rolls: Vec<(String, u32)> = throws
        .into_iter()
        .map(|(name, rolls)| (name, rolls.iter().sum::<u32>()))
        .collect();

    rolls.sort_by(|a, b| b.1.cmp(&a.1));

    Ok(rolls)
}

fn get_helper_message(responses: &[String]) -> String {
    format!(
        "Kindly go and make *{}* cups of tea. {}",
        responses.len(),
        if responses.contains(&"alexander.stepanov".to_owned()) {
            "\n\nPlease make sure that one tea is a lemon special! 🍋 \n\n"
        } else {
            ""
        }
    )
}

async fn tea_timer(num_tea: usize) -> Result<(), BoxError> {
    let (timestamp, channel_id) = send_slack_message(&format!(
        "Tea timer started: {} minutes left to brew.",
        TEA_TIMER_DURATION_MINUTES
    ))
    .await?;

    let total_seconds = TEA_TIMER_DURATION_MINUTES * 60;
    for seconds_left in (0..total_seconds).rev().step_by(15) {
        tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;

        if ACTIVE_TEA_OFFER.lock().unwrap().is_none() {
            break;
        }

        let minutes = seconds_left / 60;
        let secs = seconds_left % 60;
        update_slack_message(
            &format!("Tea timer started: {}:{:02} left to brew.", minutes, secs),
            &timestamp,
            &channel_id,
        )
        .await?;
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

const STATS_FILE: &str = "/data/tea_stats.json";

async fn load_stats_from_file() {
    if Path::new(STATS_FILE).exists() {
        match fs::read_to_string(STATS_FILE) {
            Ok(content) => {
                if let Ok(stats_map) = serde_json::from_str::<HashMap<String, UserStats>>(&content)
                {
                    *USER_STATS.lock().unwrap() = stats_map;
                    println!("Loaded tea statistics from file");
                } else {
                    eprintln!("Failed to parse stats file");
                }
            }
            Err(e) => eprintln!("Failed to read stats file: {}", e),
        }
    }
}

async fn save_stats_to_file() -> Result<(), BoxError> {
    let stats = USER_STATS.lock().unwrap().clone();
    let json_content = serde_json::to_string_pretty(&stats)?;
    fs::write(STATS_FILE, json_content)?;
    Ok(())
}

fn get_user_stats(username: &str) -> UserStats {
    USER_STATS
        .lock()
        .unwrap()
        .get(username)
        .cloned()
        .unwrap_or_else(|| UserStats {
            username: username.to_string(),
            ..Default::default()
        })
}

async fn update_user_stats(stats: UserStats) -> Result<(), BoxError> {
    {
        let mut stats_map = USER_STATS.lock().unwrap();
        stats_map.insert(stats.username.clone(), stats);
    }
    save_stats_to_file().await?;
    Ok(())
}

async fn update_participation_stats(
    participants: &[String],
    tea_maker: &str,
    king: Option<&str>,
    bitch: Option<&str>,
) -> Result<(), BoxError> {
    for participant in participants {
        let mut stats = get_user_stats(participant);
        stats.rounds_participated += 1;

        if participant == tea_maker {
            stats.times_lost += 1;
            stats.teas_made += participants.len() as u32;
        }

        if let Some(king_name) = king {
            if participant == king_name {
                stats.times_king += 1;
            }
        }

        if let Some(bitch_name) = bitch {
            if participant == bitch_name {
                stats.times_bitch += 1;
            }
        }

        update_user_stats(stats).await?;
    }
    Ok(())
}

async fn update_request_or_offer_stats(username: &str, is_request: bool) -> Result<(), BoxError> {
    let mut stats = get_user_stats(username);
    if is_request {
        stats.times_requested += 1;
    } else {
        stats.times_offered += 1;
    }
    update_user_stats(stats).await?;
    Ok(())
}

async fn generate_leaderboard() -> Result<String, BoxError> {
    let stats_map = USER_STATS.lock().unwrap().clone();

    if stats_map.is_empty() {
        return Ok("📊 *Tea Leaderboard*\n\nNo tea statistics yet! Start a tea round to begin tracking stats.".to_string());
    }

    let mut stats_vec: Vec<UserStats> = stats_map.into_values().collect();

    stats_vec.sort_by(|a, b| b.teas_made.cmp(&a.teas_made));

    let mut leaderboard = String::from("📊 *Tea Leaderboard*\n\n");

    for (i, stats) in stats_vec.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "  ",
        };

        leaderboard.push_str(&format!(
            "{} *{}*\n   teas made: {} | rounds: {} | rt: {} | ot: {} | lost: {} | king: {} | bitch: {}\n\n",
            medal,
            stats.username,
            stats.teas_made,
            stats.rounds_participated,
            stats.times_requested,
            stats.times_offered,
            if stats.times_lost >= stats.times_offered { stats.times_lost - stats.times_offered } else { 0 },
            stats.times_king,
            stats.times_bitch,
        ));
    }

    Ok(leaderboard)
}

async fn send_slack_image(image_url: &str) -> Result<(), BoxError> {
    let slack_token = env::var("SLACK_BOT_TOKEN").expect("SLACK_BOT_TOKEN must be set");
    let client = reqwest::Client::new();

    let payload = json!({
        "channel": "t",
        "blocks": [
            {
                "type": "image",
                "image_url": image_url,
                "alt_text": "Special tea event image"
            }
        ]
    });

    client
        .post("https://slack.com/api/chat.postMessage")
        .header("Authorization", format!("Bearer {}", slack_token))
        .json(&payload)
        .send()
        .await?;

    Ok(())
}
