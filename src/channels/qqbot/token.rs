//! Token state machine and background refresh for QQ Bot OAuth tokens.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info};

use super::channel::TOKEN_URL;
use super::channel::user_agent;

#[derive(Clone)]
pub struct TokenState {
    pub access_token: String,
    /// Wall-clock expiry time. Uses `SystemTime` instead of `Instant` so that
    /// token expiry is correctly detected after system suspend (e.g. laptop
    /// sleep). NTP adjustments of a few seconds are negligible compared to the
    /// typical ~2-hour token lifetime.
    pub expires_at: std::time::SystemTime,
}

pub struct TokenManager {
    pub state: tokio::sync::RwLock<Option<TokenState>>,
    pub app_id: String,
    pub client_secret: String,
    pub http_client: reqwest::Client,
    /// Background refresh task handle.
    pub bg_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Serializes token refreshes (singleflight). Concurrent refreshers share
    /// one network fetch instead of each issuing a new token — important
    /// because QQ's API may invalidate the previously-issued token, and racing
    /// fetches can leave the cache holding an already-invalidated token.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl TokenManager {
    pub fn new(app_id: String, client_secret: String) -> Self {
        Self {
            state: tokio::sync::RwLock::new(None),
            app_id,
            client_secret,
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            bg_handle: tokio::sync::Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Start the background token refresh loop.
    /// Refreshes the token before it expires, so callers always get a valid token.
    pub async fn start_background_refresh(self: &Arc<Self>) {
        let mut handle = self.bg_handle.lock().await;
        if handle.is_some() {
            return; // Already running.
        }
        // Synchronously fetch the first token before spawning the background loop,
        // so that listen() returns with a populated cache and get_token() won't
        // race with a fallback do_refresh().
        if let Err(e) = self.do_refresh().await {
            error!(err = %e, "QQ Bot initial token fetch failed");
        }
        let this = Arc::clone(self);
        *handle = Some(tokio::spawn(async move {
            this.background_refresh_loop().await;
        }));
    }

    /// Background loop: refresh token before expiry.
    /// The initial fetch is already done by `start_background_refresh`; this loop
    /// handles ongoing periodic refresh.  If the initial fetch failed, the token
    /// cache is still empty, so the first sleep_duration will be 5 s and the loop
    /// will retry naturally.
    async fn background_refresh_loop(&self) {
        loop {
            // Calculate sleep duration until next refresh.
            let sleep_duration = {
                let state = self.state.read().await;
                match *state {
                    Some(ref s) => {
                        let remaining = s
                            .expires_at
                            .duration_since(std::time::SystemTime::now())
                            .unwrap_or(Duration::ZERO);
                        // Refresh early — but never more than 1/3 of the remaining lifetime,
                        // so short-lived tokens still get a reasonable sleep window.
                        let refresh_ahead = Duration::min(Duration::from_secs(300), remaining / 3);
                        let jitter = Duration::from_millis(rand::random::<u64>() % 30_000);
                        remaining
                            .saturating_sub(refresh_ahead)
                            .saturating_sub(jitter)
                    }
                    None => Duration::from_secs(5), // No token, retry soon.
                }
            };

            if !sleep_duration.is_zero() {
                tokio::time::sleep(sleep_duration).await;
            }

            if let Err(e) = self.do_refresh().await {
                error!(err = %e, "QQ Bot background token refresh failed, retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    /// Get a valid access token from cache, refreshing automatically if expired.
    /// The background task proactively refreshes tokens before expiry; this
    /// method acts as a safety net for the race where the cached token has
    /// expired between background refresh cycles.
    pub async fn get_token(&self) -> anyhow::Result<String> {
        {
            let state = self.state.read().await;
            if let Some(ref s) = *state {
                if s.expires_at > std::time::SystemTime::now() {
                    return Ok(s.access_token.clone());
                }
            }
        }
        // Token expired or not yet initialized — refresh now.
        self.do_refresh().await
    }

    /// Force refresh the access token.
    ///
    /// Always fetches a new token, ignoring any cached value. Used when the QQ
    /// API signals the current token is invalid (HTTP 401 / code 11244) even if
    /// our local clock says it hasn't expired. The refresh lock serializes
    /// concurrent force-refreshes so only one network fetch happens at a time.
    pub async fn refresh(&self) -> anyhow::Result<String> {
        let _guard = self.refresh_lock.lock().await;
        let token_state = self.fetch_new_token().await?;
        let token = token_state.access_token.clone();
        *self.state.write().await = Some(token_state);
        Ok(token)
    }

    /// Internal: get a valid token, refreshing if needed.
    ///
    /// Singleflight: the refresh lock serializes refreshes. After acquiring the
    /// lock we re-check the cache — another caller may have just refreshed — so
    /// concurrent callers share one fetch instead of each hitting the API.
    /// Fetching a new QQ token can invalidate the previous one, so deduplication
    /// also avoids the "second fetch invalidates the first, cache holds stale
    /// token" race.
    pub async fn do_refresh(&self) -> anyhow::Result<String> {
        let _guard = self.refresh_lock.lock().await;
        // Re-check after acquiring the lock: a concurrent refresh may have just
        // populated the cache with a still-fresh token.
        {
            let state = self.state.read().await;
            if let Some(ref s) = *state {
                if let Ok(remaining) = s.expires_at.duration_since(std::time::SystemTime::now()) {
                    if remaining > Duration::from_secs(5) {
                        return Ok(s.access_token.clone());
                    }
                }
            }
        }
        let token_state = self.fetch_new_token().await?;
        let token = token_state.access_token.clone();
        *self.state.write().await = Some(token_state);
        Ok(token)
    }

    /// Actually fetch a new token from the API.
    async fn fetch_new_token(&self) -> anyhow::Result<TokenState> {
        let body = serde_json::json!({
            "appId": self.app_id,
            "clientSecret": self.client_secret,
        });

        let ua = user_agent();
        let resp = self
            .http_client
            .post(TOKEN_URL)
            .json(&body)
            .header("User-Agent", &ua)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("token request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "token request returned {}: {}",
                status,
                text
            ));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("token parse error: {}", e))?;

        let access_token = data["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing access_token in response"))?
            .to_string();

        let expires_in: u64 = data["expires_in"].as_u64().unwrap_or(7000);

        let token_state = TokenState {
            access_token: access_token.clone(),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(expires_in),
        };

        info!(
            expires_in_secs = expires_in,
            "QQ Bot access token refreshed"
        );
        Ok(token_state)
    }
}
