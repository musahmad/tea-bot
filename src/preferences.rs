use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Timelike, Utc};
use chrono_tz::Europe::London;
use reqwest::Client;
use serde_json::{json, Value};

use crate::FirestoreConfig;

/// Collection storing one tea-preference document per Slack user id. Separate
/// from the terms and rounds collections so they never collide.
const PREFERENCES_COLLECTION: &str = "tea_preferences";

/// Collection holding admin-configurable bot config. Currently just the master
/// tea-options list, in the single document `tea_config/options`.
const CONFIG_COLLECTION: &str = "tea_config";
const OPTIONS_DOC: &str = "options";

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Teas offered when the Firestore options document is missing or unreadable.
/// This is also the list an admin would start from when seeding `tea_config/options`.
pub const DEFAULT_TEA_OPTIONS: [&str; 6] = [
    "Normal",
    "Decaf",
    "Decaf Oatmilk",
    "Normal Lemon",
    "Decaf Lemon",
    "Normal Oatmilk",
];

/// Switchover used when a user hasn't picked one yet.
pub const DEFAULT_SWITCH_TIME: &str = "12:30";
const DEFAULT_SWITCH_MINUTES: u32 = 12 * 60 + 30;

/// Tea assumed for a user who hasn't set a preference (or the relevant slot).
pub const DEFAULT_TEA: &str = "Normal";

/// Which of a user's two teas a value refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeaSlot {
    /// Served before the user's switchover time.
    Morning,
    /// Served at/after the user's switchover time.
    Afternoon,
}

impl TeaSlot {
    /// Firestore field name for this slot.
    pub fn field(self) -> &'static str {
        match self {
            TeaSlot::Morning => "morning_tea",
            TeaSlot::Afternoon => "afternoon_tea",
        }
    }

    pub fn from_action_id(action_id: &str) -> Option<Self> {
        match action_id {
            "pref_morning_tea" => Some(TeaSlot::Morning),
            "pref_afternoon_tea" => Some(TeaSlot::Afternoon),
            _ => None,
        }
    }
}

/// A user's saved teas and switchover time. Any field is `None` until set.
#[derive(Debug, Clone, Default)]
pub struct TeaPreference {
    pub morning_tea: Option<String>,
    pub afternoon_tea: Option<String>,
    /// "HH:MM" (24h). `None` falls back to [`DEFAULT_SWITCH_TIME`].
    pub switch_time: Option<String>,
}

impl TeaPreference {
    pub fn tea(&self, slot: TeaSlot) -> Option<&str> {
        match slot {
            TeaSlot::Morning => self.morning_tea.as_deref(),
            TeaSlot::Afternoon => self.afternoon_tea.as_deref(),
        }
    }

    /// Switchover time in minutes since midnight, defaulting when unset/malformed.
    fn switch_minutes(&self) -> u32 {
        self.switch_time
            .as_deref()
            .and_then(parse_hhmm)
            .unwrap_or(DEFAULT_SWITCH_MINUTES)
    }

    /// The tea to make given the time of day (minutes since midnight). Prefers
    /// the active slot, falls back to whichever single tea is set, and finally to
    /// [`DEFAULT_TEA`] when the user has set nothing.
    pub fn resolve(&self, now_minutes: u32) -> &str {
        let chosen = if now_minutes < self.switch_minutes() {
            self.morning_tea.as_deref()
        } else {
            self.afternoon_tea.as_deref()
        };
        chosen
            .or(self.morning_tea.as_deref())
            .or(self.afternoon_tea.as_deref())
            .unwrap_or(DEFAULT_TEA)
    }
}

/// Parse "HH:MM" into minutes since midnight.
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// The current time of day in Europe/London, as minutes since midnight. Used to
/// decide each user's active tea. Handles BST/GMT automatically.
pub fn london_now_minutes() -> u32 {
    let now = Utc::now().with_timezone(&London);
    now.hour() * 60 + now.minute()
}

/// Durable per-user tea preferences plus the admin-configurable options list,
/// backed by Firestore (native mode). Mirrors `TermsStore`/`RoundStore`: on
/// Cloud Run it authenticates via the instance metadata server, and an empty
/// project id disables persistence — preferences then live only in memory so the
/// picker still works in local dev.
pub struct PreferenceStore {
    client: Client,
    firestore: Option<FirestoreConfig>,
    /// In-memory store used when Firestore is not configured (local dev).
    local: Mutex<HashMap<String, TeaPreference>>,
}

impl PreferenceStore {
    pub fn new(firestore: Option<FirestoreConfig>) -> Self {
        // An empty project id means "not configured" — same convention as the
        // other stores.
        let firestore = firestore.filter(|f| !f.project.trim().is_empty());

        match firestore.as_ref() {
            None => tracing::warn!(
                "PreferenceStore: no firestore config. Tea preferences kept in memory only (local dev)."
            ),
            Some(f) => tracing::info!(
                "PreferenceStore: storing preferences in firestore {}/{}/{}",
                f.project,
                f.database,
                PREFERENCES_COLLECTION
            ),
        }

        Self {
            client: Client::new(),
            firestore,
            local: Mutex::new(HashMap::new()),
        }
    }

    fn document_url(&self, fs: &FirestoreConfig, collection: &str, doc: &str) -> String {
        format!(
            "https://firestore.googleapis.com/v1/projects/{}/databases/{}/documents/{}/{}",
            fs.project, fs.database, collection, doc
        )
    }

    async fn access_token(&self) -> Option<String> {
        self.client
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| tracing::error!("PreferenceStore: failed to fetch access token: {}", e))
            .ok()?
            .json::<TokenResponse>()
            .await
            .map_err(|e| tracing::error!("PreferenceStore: failed to parse token response: {}", e))
            .ok()
            .map(|t| t.access_token)
    }

    /// The admin-configurable list of teas. Reads `tea_config/options` live so
    /// changes in the Firestore console take effect without a redeploy. Falls
    /// back to [`DEFAULT_TEA_OPTIONS`] when unset or on any error.
    pub async fn options(&self) -> Vec<String> {
        let default = || DEFAULT_TEA_OPTIONS.iter().map(|s| s.to_string()).collect();

        let Some(firestore) = self.firestore.as_ref() else {
            return default();
        };
        let Some(token) = self.access_token().await else {
            return default();
        };

        let resp = match self
            .client
            .get(self.document_url(firestore, CONFIG_COLLECTION, OPTIONS_DOC))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("PreferenceStore: options read failed: {}", e);
                return default();
            }
        };

        if !resp.status().is_success() {
            // NOT_FOUND (never seeded) or anything else: fall back to defaults.
            return default();
        }

        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("PreferenceStore: bad options body: {}", e);
                return default();
            }
        };

        let parsed: Vec<String> = body
            .get("fields")
            .and_then(|f| f.get("options"))
            .and_then(|v| v.get("arrayValue"))
            .and_then(|a| a.get("values"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.get("stringValue").and_then(|s| s.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        if parsed.is_empty() {
            default()
        } else {
            parsed
        }
    }

    /// The user's saved preferences. Returns an empty (all-`None`) preference on
    /// a missing document or any transient error, so the picker always renders.
    pub async fn get(&self, slack_id: &str) -> TeaPreference {
        let Some(firestore) = self.firestore.as_ref() else {
            return self
                .local
                .lock()
                .unwrap()
                .get(slack_id)
                .cloned()
                .unwrap_or_default();
        };

        let Some(token) = self.access_token().await else {
            tracing::error!(
                "PreferenceStore: no access token; returning empty prefs for {}",
                slack_id
            );
            return TeaPreference::default();
        };

        let resp = match self
            .client
            .get(self.document_url(firestore, PREFERENCES_COLLECTION, slack_id))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("PreferenceStore: read failed for {}: {}", slack_id, e);
                return TeaPreference::default();
            }
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return TeaPreference::default();
        }
        if !resp.status().is_success() {
            tracing::error!(
                "PreferenceStore: unexpected read status {} for {}",
                resp.status(),
                slack_id
            );
            return TeaPreference::default();
        }

        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("PreferenceStore: bad read body for {}: {}", slack_id, e);
                return TeaPreference::default();
            }
        };

        let read = |field: &str| {
            body.get("fields")
                .and_then(|f| f.get(field))
                .and_then(|v| v.get("stringValue"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };

        TeaPreference {
            morning_tea: read("morning_tea"),
            afternoon_tea: read("afternoon_tea"),
            switch_time: read("switch_time"),
        }
    }

    /// Persist one tea slot without touching the other fields.
    pub async fn set_tea(&self, slack_id: &str, slot: TeaSlot, value: &str) -> bool {
        self.set_field(slack_id, slot.field(), value, |pref| match slot {
            TeaSlot::Morning => pref.morning_tea = Some(value.to_string()),
            TeaSlot::Afternoon => pref.afternoon_tea = Some(value.to_string()),
        })
        .await
    }

    /// Persist the switchover time ("HH:MM") without touching the teas.
    pub async fn set_switch_time(&self, slack_id: &str, hhmm: &str) -> bool {
        self.set_field(slack_id, "switch_time", hhmm, |pref| {
            pref.switch_time = Some(hhmm.to_string())
        })
        .await
    }

    /// Write a single field via Firestore `updateMask` (so sibling fields are
    /// preserved), or apply `local_update` to the in-memory store in local dev.
    async fn set_field(
        &self,
        slack_id: &str,
        field: &str,
        value: &str,
        local_update: impl FnOnce(&mut TeaPreference),
    ) -> bool {
        let Some(firestore) = self.firestore.as_ref() else {
            let mut local = self.local.lock().unwrap();
            local_update(local.entry(slack_id.to_string()).or_default());
            return true;
        };

        let Some(token) = self.access_token().await else {
            return false;
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let doc = json!({
            "fields": {
                field: { "stringValue": value },
                "updated_at_unix": { "integerValue": now.to_string() },
            }
        });

        // updateMask limits the write to just this field (+ timestamp) so the
        // rest of the document is preserved. PATCH also creates the document if
        // it doesn't exist yet.
        let url = format!(
            "{}?updateMask.fieldPaths={}&updateMask.fieldPaths=updated_at_unix",
            self.document_url(firestore, PREFERENCES_COLLECTION, slack_id),
            field
        );

        match self
            .client
            .patch(url)
            .bearer_auth(token)
            .json(&doc)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                tracing::error!(
                    "PreferenceStore: write failed ({}) for {}: {}",
                    status,
                    slack_id,
                    body
                );
                false
            }
            Err(e) => {
                tracing::error!("PreferenceStore: write error for {}: {}", slack_id, e);
                false
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
}
