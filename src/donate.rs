//! `/t donate`: an interactive form to transfer TEA from the invoking user to
//! another tea-bot user. Slack renders the form as an ephemeral message; the
//! "Donate" button submits the picked recipient, typed amount and optional
//! message, all read back from the block-actions `state.values`.

use serde_json::{json, Value};

use crate::User;

pub const RECIPIENT_BLOCK: &str = "donate_recipient_block";
pub const RECIPIENT_ACTION: &str = "donate_recipient";
pub const AMOUNT_BLOCK: &str = "donate_amount_block";
pub const AMOUNT_ACTION: &str = "donate_amount";
pub const MESSAGE_BLOCK: &str = "donate_message_block";
pub const MESSAGE_ACTION: &str = "donate_message";
pub const SUBMIT_ACTION: &str = "donate_submit";

/// Recipient the dropdown pre-selects when the donor hasn't picked one yet.
const DEFAULT_RECIPIENT_NAME: &str = "Musa";

/// Slack enforces this on the input, so the announcement can't be flooded.
pub const MAX_MESSAGE_LEN: usize = 140;

/// The donation form: a recipient dropdown (all tea-bot users except the donor),
/// a freeform amount input, an optional message and a submit button.
/// `selected_recipient`/`amount`/`message` pre-fill the controls when
/// re-rendering after a validation error, so the user's choices survive.
/// `notice` shows a validation line when present.
pub fn donate_blocks(
    users: &[User],
    donor_id: &str,
    selected_recipient: Option<&str>,
    amount: Option<&str>,
    message: Option<&str>,
    notice: Option<&str>,
) -> Value {
    let options: Vec<Value> = users
        .iter()
        .filter(|u| u.id != donor_id)
        .map(user_option)
        .collect();

    let mut recipient_select = json!({
        "type": "static_select",
        "action_id": RECIPIENT_ACTION,
        "placeholder": { "type": "plain_text", "text": "Pick a user", "emoji": true },
        "options": options,
    });
    // The donor's explicit pick wins; otherwise default to Musa (unless that's
    // the donor). `find` skips the donor either way, so self can't be selected.
    let default_recipient =
        users.iter().find(|u| u.name.eq_ignore_ascii_case(DEFAULT_RECIPIENT_NAME));
    if let Some(user) = selected_recipient
        .and_then(|id| users.iter().find(|u| u.id == id))
        .or(default_recipient)
        .filter(|u| u.id != donor_id)
    {
        recipient_select["initial_option"] = user_option(user);
    }

    let mut amount_input = json!({
        "type": "plain_text_input",
        "action_id": AMOUNT_ACTION,
        "placeholder": { "type": "plain_text", "text": "e.g. 5", "emoji": true },
    });
    if let Some(amount) = amount.filter(|a| !a.is_empty()) {
        amount_input["initial_value"] = json!(amount);
    }

    let mut message_input = json!({
        "type": "plain_text_input",
        "action_id": MESSAGE_ACTION,
        "max_length": MAX_MESSAGE_LEN,
        "placeholder": { "type": "plain_text", "text": "e.g. thanks for the brew!", "emoji": true },
    });
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        message_input["initial_value"] = json!(message);
    }

    let mut blocks = vec![
        json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": "💸 *Donate TEA*" }
        }),
        json!({
            "type": "section",
            "block_id": RECIPIENT_BLOCK,
            "text": { "type": "mrkdwn", "text": "*Recipient*" },
            "accessory": recipient_select,
        }),
        json!({
            "type": "input",
            "block_id": AMOUNT_BLOCK,
            "optional": true,
            "label": { "type": "plain_text", "text": "Amount (TEA)", "emoji": true },
            "element": amount_input,
        }),
        json!({
            "type": "input",
            "block_id": MESSAGE_BLOCK,
            "optional": true,
            "label": { "type": "plain_text", "text": "Message (optional)", "emoji": true },
            "element": message_input,
        }),
        json!({
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "text": { "type": "plain_text", "text": "Donate", "emoji": true },
                    "style": "primary",
                    "action_id": SUBMIT_ACTION,
                }
            ]
        }),
    ];

    if let Some(notice) = notice {
        blocks.push(json!({
            "type": "context",
            "elements": [ { "type": "mrkdwn", "text": notice } ]
        }));
    }

    json!(blocks)
}

fn user_option(user: &User) -> Value {
    json!({
        "text": { "type": "plain_text", "text": user.name, "emoji": true },
        "value": user.id,
    })
}

/// Pull the picked recipient id, raw amount text and optional message out of a
/// block-actions `state.values` object. Each is `None` when the user left that
/// control untouched.
pub fn parse_submission(values: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let recipient = values
        .get(RECIPIENT_BLOCK)
        .and_then(|b| b.get(RECIPIENT_ACTION))
        .and_then(|a| a.get("selected_option"))
        .and_then(|o| o.get("value"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let amount = values
        .get(AMOUNT_BLOCK)
        .and_then(|b| b.get(AMOUNT_ACTION))
        .and_then(|a| a.get("value"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let message = values
        .get(MESSAGE_BLOCK)
        .and_then(|b| b.get(MESSAGE_ACTION))
        .and_then(|a| a.get("value"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    (recipient, amount, message)
}
