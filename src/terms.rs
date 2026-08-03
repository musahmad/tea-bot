use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::FirestoreConfig;

/// Bump this whenever the terms change — every user must re-accept the new version.
pub const TERMS_VERSION: &str = "2026-08-03";

pub const TERMS_TEXT: &str = "\
By placing a bid you agree to the Tea-Bot Terms & Conditions:

1. You only bid when you genuinely want and agree to accept a cup of hot tea. Variations (decaf, lemon, oat milk) are acceptable, but cold drinks and other kitchen items are not.
2. If you bid the lowest, you make the tea, promptly.
3. Bids are blind and locked the moment you place them.

Failure to follow these rules may result in a TEA penalty, or temporary suspension at the discretion of the Tea Administration.
";

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Durable record of which users have accepted the current terms, backed by
/// Firestore (native mode). On Cloud Run this authenticates via the instance
/// metadata server using the service account — no key files required.
pub struct TermsStore {
    client: Client,
    /// `None` (or a config with an empty project) disables enforcement, e.g.
    /// local dev with no GCP project.
    firestore: Option<FirestoreConfig>,
    /// Slack ids known to have accepted the current `TERMS_VERSION`. Acceptance
    /// is monotonic per version, so caching a `true` is always safe.
    accepted_cache: Mutex<HashSet<String>>,
}

impl TermsStore {
    pub async fn new(firestore: Option<FirestoreConfig>) -> Self {
        // Treat an empty project id as "not configured" so local runs aren't gated.
        let firestore = firestore.filter(|f| !f.project.trim().is_empty());

        match firestore.as_ref() {
            None => tracing::warn!(
                "TermsStore: no firestore config. Terms enforcement DISABLED — bids will not be gated."
            ),
            Some(f) => tracing::info!(
                "TermsStore: enforcing terms v{} against firestore {}/{}/{}",
                TERMS_VERSION,
                f.project,
                f.database,
                f.collection
            ),
        }

        Self {
            client: Client::new(),
            firestore,
            accepted_cache: Mutex::new(HashSet::new()),
        }
    }

    fn document_url(&self, fs: &FirestoreConfig, slack_id: &str) -> String {
        format!(
            "https://firestore.googleapis.com/v1/projects/{}/databases/{}/documents/{}/{}",
            fs.project, fs.database, fs.collection, slack_id
        )
    }

    async fn access_token(&self) -> Option<String> {
        self.client
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| tracing::error!("TermsStore: failed to fetch access token: {}", e))
            .ok()?
            .json::<TokenResponse>()
            .await
            .map_err(|e| tracing::error!("TermsStore: failed to parse token response: {}", e))
            .ok()
            .map(|t| t.access_token)
    }

    /// Whether `slack_id` has accepted the current `TERMS_VERSION`.
    ///
    /// Returns `true` when enforcement is disabled (no project id), and fails
    /// open on transient Firestore/token errors so an outage cannot block the
    /// whole game. A definitive "not found" or stale version returns `false`.
    pub async fn has_accepted(&self, slack_id: &str) -> bool {
        let Some(firestore) = self.firestore.as_ref() else {
            return true; // enforcement disabled (local dev)
        };

        {
            if self.accepted_cache.lock().unwrap().contains(slack_id) {
                return true;
            }
        }

        let Some(token) = self.access_token().await else {
            tracing::error!("TermsStore: no access token; failing open for {}", slack_id);
            return true;
        };

        let resp = match self
            .client
            .get(self.document_url(firestore, slack_id))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("TermsStore: read failed for {}: {}; failing open", slack_id, e);
                return true;
            }
        };

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return false;
        }
        if !resp.status().is_success() {
            tracing::error!(
                "TermsStore: unexpected read status {} for {}; failing open",
                resp.status(),
                slack_id
            );
            return true;
        }

        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("TermsStore: bad read body for {}: {}; failing open", slack_id, e);
                return true;
            }
        };

        let accepted_version = body
            .get("fields")
            .and_then(|f| f.get("version"))
            .and_then(|v| v.get("stringValue"))
            .and_then(|v| v.as_str());

        if accepted_version == Some(TERMS_VERSION) {
            self.accepted_cache
                .lock()
                .unwrap()
                .insert(slack_id.to_string());
            true
        } else {
            false
        }
    }

    /// Persists that `slack_id` accepted the current `TERMS_VERSION`. Returns
    /// `true` on success. With enforcement disabled it just updates the cache.
    pub async fn record_acceptance(&self, slack_id: &str) -> bool {
        let Some(firestore) = self.firestore.as_ref() else {
            self.accepted_cache
                .lock()
                .unwrap()
                .insert(slack_id.to_string());
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
                "version": { "stringValue": TERMS_VERSION },
                "accepted_at_unix": { "integerValue": now.to_string() },
                "method": { "stringValue": "button" },
            }
        });

        match self
            .client
            .patch(self.document_url(firestore, slack_id))
            .bearer_auth(token)
            .json(&doc)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                self.accepted_cache
                    .lock()
                    .unwrap()
                    .insert(slack_id.to_string());
                true
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                tracing::error!(
                    "TermsStore: write failed ({}) for {}: {}",
                    status,
                    slack_id,
                    body
                );
                false
            }
            Err(e) => {
                tracing::error!("TermsStore: write error for {}: {}", slack_id, e);
                false
            }
        }
    }
}
