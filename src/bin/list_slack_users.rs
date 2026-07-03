// Usage:
//   cargo run --bin list_slack_users            # real users only
//   cargo run --bin list_slack_users -- --all   # include bots and deleted users

use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    slack_bot_token: String,
}

#[derive(Debug, Deserialize)]
struct SlackUsersResponse {
    ok: bool,
    members: Option<Vec<SlackMember>>,
    error: Option<String>,
    response_metadata: Option<ResponseMetadata>,
}

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackMember {
    id: String,
    name: String,
    real_name: Option<String>,
    is_bot: Option<bool>,
    deleted: Option<bool>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    let config: Config = load_config()?;
    let client = reqwest::Client::new();

    let mut cursor = String::new();
    let mut all_members: Vec<SlackMember> = Vec::new();

    loop {
        let mut req = client
            .get("https://slack.com/api/users.list")
            .header("Authorization", format!("Bearer {}", config.slack_bot_token))
            .query(&[("limit", "200")]);

        if !cursor.is_empty() {
            req = req.query(&[("cursor", cursor.as_str())]);
        }

        let resp: SlackUsersResponse = req.send().await?.json().await?;

        if !resp.ok {
            anyhow::bail!("Slack API error: {}", resp.error.unwrap_or_default());
        }

        if let Some(members) = resp.members {
            all_members.extend(members);
        }

        match resp.response_metadata.and_then(|m| m.next_cursor) {
            Some(c) if !c.is_empty() => cursor = c,
            _ => break,
        }
    }

    // Filter out bots and deleted users by default
    let show_all = std::env::args().any(|a| a == "--all");

    println!("{:<15} {:<25} {}", "SLACK ID", "USERNAME", "REAL NAME");
    println!("{}", "-".repeat(65));

    for member in &all_members {
        let is_bot = member.is_bot.unwrap_or(false);
        let deleted = member.deleted.unwrap_or(false);

        if !show_all && (is_bot || deleted) {
            continue;
        }

        println!(
            "{:<15} {:<25} {}",
            member.id,
            member.name,
            member.real_name.as_deref().unwrap_or("")
        );
    }

    println!("\nTotal: {} users", all_members.iter().filter(|m| {
        show_all || (!m.is_bot.unwrap_or(false) && !m.deleted.unwrap_or(false))
    }).count());

    Ok(())
}

fn load_config() -> anyhow::Result<Config> {
    if let Ok(json) = std::env::var("CONFIG_JSON") {
        Ok(serde_json5::from_str(&json)?)
    } else {
        let path = std::env::var("CONFIG_PATH")
            .unwrap_or_else(|_| "config.json5".to_string());
        let mut file = std::fs::File::open(&path)?;
        Ok(serde_json5::from_reader(&mut file)?)
    }
}
