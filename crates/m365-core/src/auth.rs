//! OAuth2 device-code authentication against Entra ID, with a small on-disk
//! token cache and silent refresh.
//!
//! There is no first-party MSAL for Rust, but the device-code flow is simple:
//! POST the desired scopes to the `/devicecode` endpoint, show the returned
//! `user_code` + `verification_uri` to the user, then poll the `/token`
//! endpoint until they finish signing in. Refresh tokens (via the
//! `offline_access` scope) then keep the session alive without further
//! interaction.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{de, Deserialize, Deserializer, Serialize};
use tokio::sync::Mutex;

use crate::config::Config;

/// Prompt shown to the user to complete the device-code login.
#[derive(Debug, Clone)]
pub struct DeviceCodePrompt {
    pub verification_uri: String,
    pub user_code: String,
    /// Human-readable message from Entra (already contains the URL + code).
    pub message: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(alias = "_expires_at", deserialize_with = "deserialize_datetime")]
    expires_at: DateTime<Utc>,
    #[serde(default)]
    scope: String,
}

impl CachedToken {
    /// Consider the access token expired 60s early to avoid mid-request expiry.
    fn is_valid(&self) -> bool {
        Utc::now() + chrono::Duration::seconds(60) < self.expires_at
    }
}

pub struct Authenticator {
    http: reqwest::Client,
    client_id: String,
    devicecode_endpoint: String,
    token_endpoint: String,
    scope: String,
    cache_path: PathBuf,
    refresh_command: Option<String>,
    state: Mutex<Option<CachedToken>>,
}

impl Authenticator {
    pub fn new(config: &Config) -> Self {
        let cached = load_cache(&config.token_cache_path);
        Self {
            http: reqwest::Client::new(),
            client_id: config.client_id.clone(),
            devicecode_endpoint: config.devicecode_endpoint(),
            token_endpoint: config.token_endpoint(),
            scope: config.scope_string(),
            cache_path: config.token_cache_path.clone(),
            refresh_command: config.token_refresh_command.clone(),
            state: Mutex::new(cached),
        }
    }

    /// True if we have any cached credentials (valid or refreshable).
    pub async fn has_credentials(&self) -> bool {
        self.state.lock().await.is_some()
    }

    /// Return a valid access token, refreshing silently if needed. Errors if
    /// there are no cached credentials — call [`Authenticator::login`] first.
    pub async fn access_token(&self) -> Result<String> {
        let mut guard = self.state.lock().await;
        let current = guard
            .as_ref()
            .ok_or_else(|| anyhow!("not signed in; run device-code login first"))?;

        if current.is_valid() {
            return Ok(current.access_token.clone());
        }

        let refresh = current
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("access token expired and no refresh token available"))?;

        let refreshed = if let Some(command) = &self.refresh_command {
            self.refresh_externally(command).await?
        } else {
            self.refresh(&refresh).await?
        };
        let token = refreshed.access_token.clone();
        if self.refresh_command.is_none() {
            self.persist(&refreshed);
        }
        *guard = Some(refreshed);
        Ok(token)
    }

    async fn refresh_externally(&self, command: &str) -> Result<CachedToken> {
        #[cfg(windows)]
        let mut process = {
            let mut process = tokio::process::Command::new("cmd");
            process.args(["/C", command]);
            process
        };
        #[cfg(not(windows))]
        let mut process = {
            let mut process = tokio::process::Command::new("sh");
            process.args(["-c", command]);
            process
        };
        let status = process
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("running external token refresh command")?;
        if !status.success() {
            anyhow::bail!("external token refresh command failed with {status}");
        }

        let token = load_cache(&self.cache_path)
            .ok_or_else(|| anyhow!("external refresh did not write a valid token cache"))?;
        if !token.is_valid() {
            anyhow::bail!("external refresh left an expired token cache");
        }
        set_owner_only(&self.cache_path)?;
        Ok(token)
    }

    /// Run the interactive device-code flow. `on_prompt` is invoked once with
    /// the URL + code the user must enter in a browser.
    pub async fn login<F>(&self, on_prompt: F) -> Result<()>
    where
        F: FnOnce(DeviceCodePrompt),
    {
        let dc: DeviceCodeResponse = self
            .http
            .post(&self.devicecode_endpoint)
            .form(&[("client_id", self.client_id.as_str()), ("scope", &self.scope)])
            .send()
            .await
            .context("requesting device code")?
            .error_for_status()
            .context("device-code endpoint returned an error")?
            .json()
            .await
            .context("parsing device-code response")?;

        on_prompt(DeviceCodePrompt {
            verification_uri: dc.verification_uri.clone(),
            user_code: dc.user_code.clone(),
            message: dc.message.clone(),
            expires_in: dc.expires_in,
        });

        let mut interval = Duration::from_secs(dc.interval.max(1));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(dc.expires_in);

        loop {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("device-code login timed out; please try again");
            }
            tokio::time::sleep(interval).await;

            let resp = self
                .http
                .post(&self.token_endpoint)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", self.client_id.as_str()),
                    ("device_code", dc.device_code.as_str()),
                ])
                .send()
                .await
                .context("polling token endpoint")?;

            if resp.status().is_success() {
                let token: TokenResponse = resp.json().await.context("parsing token response")?;
                let cached = token.into_cached();
                self.persist(&cached);
                *self.state.lock().await = Some(cached);
                return Ok(());
            }

            let err: TokenError = resp.json().await.context("parsing token error")?;
            match err.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval += Duration::from_secs(5);
                    continue;
                }
                "authorization_declined" => anyhow::bail!("sign-in was declined by the user"),
                "expired_token" => anyhow::bail!("device code expired; please try again"),
                other => anyhow::bail!(
                    "device-code login failed: {other}: {}",
                    err.error_description.unwrap_or_default()
                ),
            }
        }
    }

    async fn refresh(&self, refresh_token: &str) -> Result<CachedToken> {
        let resp = self
            .http
            .post(&self.token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.client_id.as_str()),
                ("refresh_token", refresh_token),
                ("scope", self.scope.as_str()),
            ])
            .send()
            .await
            .context("refreshing token")?;

        if !resp.status().is_success() {
            let err: TokenError = resp.json().await.unwrap_or(TokenError {
                error: "unknown".into(),
                error_description: None,
            });
            anyhow::bail!(
                "token refresh failed ({}): {}",
                err.error,
                err.error_description.unwrap_or_default()
            );
        }

        let mut token: TokenResponse = resp.json().await.context("parsing refresh response")?;
        // Entra may omit a new refresh token; keep the old one if so.
        if token.refresh_token.is_none() {
            token.refresh_token = Some(refresh_token.to_string());
        }
        Ok(token.into_cached())
    }

    fn persist(&self, token: &CachedToken) {
        if let Err(e) = write_cache(&self.cache_path, token) {
            tracing::warn!("failed to persist token cache: {e:#}");
        }
    }
}

fn load_cache(path: &PathBuf) -> Option<CachedToken> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn write_cache(path: &PathBuf, token: &CachedToken) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_vec_pretty(token)?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    set_owner_only(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 on {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &PathBuf) -> Result<()> {
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
    message: String,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    scope: String,
}

impl TokenResponse {
    fn into_cached(self) -> CachedToken {
        CachedToken {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: Utc::now() + chrono::Duration::seconds(self.expires_in),
            scope: self.scope,
        }
    }
}

fn deserialize_datetime<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        DateTime(DateTime<Utc>),
        Unix(i64),
    }

    match Value::deserialize(deserializer)? {
        Value::DateTime(value) => Ok(value),
        Value::Unix(value) => DateTime::from_timestamp(value, 0)
            .ok_or_else(|| de::Error::custom("invalid Unix timestamp")),
    }
}

#[derive(Debug, Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_poc_token_cache() {
        let token: CachedToken = serde_json::from_str(
            r#"{
                "access_token": "access",
                "refresh_token": "refresh",
                "_expires_at": 2000000000,
                "scope": "User.Read Mail.ReadWrite"
            }"#,
        )
        .unwrap();

        assert_eq!(token.expires_at.timestamp(), 2_000_000_000);
    }
}
