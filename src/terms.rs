use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::FirestoreConfig;

const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// A single published version of the terms. The `version` is the revision
/// document's id (e.g. `2026-08-03`); the `text` is what gets shown in Slack.
#[derive(Clone, Debug)]
pub struct TermsRevision {
    pub version: String,
    pub text: String,
}

/// Durable record of the terms and who has accepted them, backed by Firestore
/// (native mode). On Cloud Run this authenticates via the instance metadata
/// server using the service account — no key files required.
///
/// Two collections are used:
/// - `revisions_collection`: one document per terms revision (admin-managed).
///   The revision with the greatest `created_at_unix` is the enforced "latest".
/// - `collection`: one acceptance document per Slack id, recording the
///   `version` that user last accepted.
pub struct TermsStore {
    client: Client,
    /// `None` (or a config with an empty project) disables enforcement, e.g.
    /// local dev with no GCP project.
    firestore: Option<FirestoreConfig>,
    /// `(version, slack_id)` pairs known to have accepted that version.
    /// Acceptance is monotonic per version, so caching a hit is always safe;
    /// keying by version means publishing a new revision correctly ignores
    /// acceptances of older versions.
    accepted_cache: Mutex<HashSet<(String, String)>>,
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
                "TermsStore: enforcing latest revision from firestore {}/{}/{}",
                f.project,
                f.database,
                f.revisions_collection
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

    fn runquery_url(&self, fs: &FirestoreConfig) -> String {
        format!(
            "https://firestore.googleapis.com/v1/projects/{}/databases/{}/documents:runQuery",
            fs.project, fs.database
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

    /// The latest terms revision (greatest `created_at_unix`).
    ///
    /// Returns `None` when enforcement is disabled, when no revision has been
    /// published yet, or on any transient Firestore/token error — all of which
    /// callers treat as "don't gate", so an outage or an unconfigured project
    /// can never block the whole game.
    pub async fn current_revision(&self) -> Option<TermsRevision> {
        let firestore = self.firestore.as_ref()?;
        let token = self.access_token().await?;

        let query = json!({
            "structuredQuery": {
                "from": [{ "collectionId": firestore.revisions_collection }],
                "orderBy": [{
                    "field": { "fieldPath": "created_at_unix" },
                    "direction": "DESCENDING"
                }],
                "limit": 1
            }
        });

        let resp = self
            .client
            .post(self.runquery_url(firestore))
            .bearer_auth(token)
            .json(&query)
            .send()
            .await
            .map_err(|e| tracing::error!("TermsStore: revision query failed: {}", e))
            .ok()?;

        if !resp.status().is_success() {
            tracing::error!(
                "TermsStore: unexpected revision-query status {}",
                resp.status()
            );
            return None;
        }

        let results: Value = resp
            .json()
            .await
            .map_err(|e| tracing::error!("TermsStore: bad revision-query body: {}", e))
            .ok()?;

        // `runQuery` returns an array; each element is either a match with a
        // `document` field or a bookkeeping entry (readTime only) when empty.
        let doc = results.as_array()?.iter().find_map(|e| e.get("document"));
        let Some(doc) = doc else {
            tracing::warn!("TermsStore: no terms revision published; not gating bids");
            return None;
        };

        // The version is the revision document's id — the last path segment of
        // its resource name.
        let version = doc
            .get("name")
            .and_then(|n| n.as_str())
            .and_then(|n| n.rsplit('/').next())
            .map(|s| s.to_string())?;
        let text = doc
            .get("fields")
            .and_then(|f| f.get("text"))
            .and_then(|t| t.get("stringValue"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())?;

        Some(TermsRevision { version, text })
    }

    /// Whether `slack_id` has accepted the given `version`.
    ///
    /// Returns `true` when enforcement is disabled (no project id), and fails
    /// open on transient Firestore/token errors so an outage cannot block the
    /// whole game. A definitive "not found" or a stale accepted version returns
    /// `false`.
    pub async fn has_accepted(&self, slack_id: &str, version: &str) -> bool {
        let Some(firestore) = self.firestore.as_ref() else {
            return true; // enforcement disabled (local dev)
        };

        let key = (version.to_string(), slack_id.to_string());
        {
            if self.accepted_cache.lock().unwrap().contains(&key) {
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

        if accepted_version == Some(version) {
            self.accepted_cache.lock().unwrap().insert(key);
            true
        } else {
            false
        }
    }

    /// Persists that `slack_id` accepted `version`. Returns `true` on success.
    /// With enforcement disabled it just updates the cache.
    pub async fn record_acceptance(&self, slack_id: &str, version: &str) -> bool {
        let key = (version.to_string(), slack_id.to_string());

        let Some(firestore) = self.firestore.as_ref() else {
            self.accepted_cache.lock().unwrap().insert(key);
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
                "version": { "stringValue": version },
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
                self.accepted_cache.lock().unwrap().insert(key);
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
