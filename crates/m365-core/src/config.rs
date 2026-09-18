//! Runtime configuration, loaded from the environment (and an optional `.env`).

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Microsoft Graph base URL (v1.0 endpoint).
pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// The Graph endpoint to talk to, overridable with `M365_GRAPH_BASE`.
///
/// Pointing this at a local mock lets the app run on fabricated data — useful
/// for recording a demo without a real mailbox on screen, and for exercising the
/// UI offline. Unset in normal use.
pub fn graph_base() -> String {
    std::env::var("M365_GRAPH_BASE")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| GRAPH_BASE.to_string())
}

/// The delegated scopes the app requests. `offline_access` is required for
/// refresh tokens; `openid`/`profile` give us the signed-in user's identity.
///
/// Keep this list stable: changing it invalidates existing consent, and in
/// tenants that disallow user consent every sign-in then needs a fresh admin
/// approval. New optional capabilities should be opt-in (see
/// [`PRESENCE_WRITE_SCOPE`]) rather than added here.
pub const DEFAULT_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "offline_access",
    "User.Read",
    "People.Read",
    "Mail.ReadWrite",
    "Mail.Send",
    "Calendars.ReadWrite",
    "Chat.ReadWrite",
    "ChannelMessage.Send",
    "ChannelMessage.Read.All",
    "Presence.Read.All",
];

/// Needed only to *set* your own presence (`setUserPreferredPresence`).
/// Opt in with `M365_PRESENCE_WRITE=1`; requires the permission to be added to
/// the app registration and consented.
pub const PRESENCE_WRITE_SCOPE: &str = "Presence.ReadWrite";

/// Needed to list the teams and channels you belong to (`/me/joinedTeams`).
/// Chats work without it. Opt in with `M365_TEAMS_CHANNELS=1`.
pub const TEAMS_READ_SCOPE: &str = "Team.ReadBasic.All";

#[derive(Debug, Clone)]
pub struct Config {
    /// Entra application (client) ID of the registered public client.
    pub client_id: String,
    /// Directory tenant ID (or `organizations` / `common`).
    pub tenant_id: String,
    /// Delegated scopes to request.
    pub scopes: Vec<String>,
    /// Public HTTPS base of the tunnel that fronts the webhook service, e.g.
    /// `https://m365.example.com`. `None` disables push (poll-only mode).
    pub tunnel_base_url: Option<String>,
    /// Redis connection URL used to receive change events from the webhook.
    pub redis_url: String,
    /// Path of the on-disk token cache (0600).
    pub token_cache_path: PathBuf,
    /// Optional external command that refreshes the shared token cache.
    pub token_refresh_command: Option<String>,
    /// Shared secret echoed in subscription `clientState` and verified by the
    /// webhook. Generated on first run if absent.
    pub client_state: String,
    /// Desktop notifications for direct messages and `@mentions`.
    pub notifications: bool,
}

impl Config {
    /// Load configuration from the process environment. Call
    /// [`Config::load_dotenv`] first if you want `.env` support.
    pub fn from_env() -> Result<Self> {
        let client_id = env_required("M365_CLIENT_ID")?;
        let tenant_id = std::env::var("M365_TENANT_ID").unwrap_or_else(|_| "organizations".into());

        let scopes = match std::env::var("M365_SCOPES") {
            Ok(s) if !s.trim().is_empty() => {
                s.split_whitespace().map(|s| s.to_string()).collect()
            }
            _ => {
                let mut s: Vec<String> = DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect();
                // Presence *writing* is opt-in: adding a scope invalidates any
                // existing consent grant, which is disruptive in tenants that
                // require admin approval.
                if env_flag("M365_PRESENCE_WRITE") {
                    s.push(PRESENCE_WRITE_SCOPE.to_string());
                }
                if env_flag("M365_TEAMS_CHANNELS") {
                    s.push(TEAMS_READ_SCOPE.to_string());
                }
                s
            }
        };

        let tunnel_base_url = std::env::var("M365_TUNNEL_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim_end_matches('/').to_string());

        let redis_url =
            std::env::var("M365_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());

        let token_cache_path = match std::env::var("M365_TOKEN_CACHE") {
            Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
            _ => default_cache_dir()?.join("token-cache.json"),
        };
        let token_refresh_command = std::env::var("M365_TOKEN_REFRESH_COMMAND")
            .ok()
            .filter(|s| !s.trim().is_empty());

        let client_state = std::env::var("M365_CLIENT_STATE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // On unless explicitly disabled.
        let notifications = !matches!(
            std::env::var("M365_NOTIFY").as_deref(),
            Ok("0") | Ok("false") | Ok("no") | Ok("off")
        );

        Ok(Self {
            client_id,
            tenant_id,
            scopes,
            tunnel_base_url,
            redis_url,
            token_cache_path,
            token_refresh_command,
            client_state,
            notifications,
        })
    }

    /// Best-effort load of a `.env` file from the current directory or nearest
    /// parent. Missing file is not an error.
    pub fn load_dotenv() {
        let _ = dotenvy::dotenv();
    }

    pub fn scope_string(&self) -> String {
        self.scopes.join(" ")
    }

    /// Whether the token we request can set presence.
    pub fn can_write_presence(&self) -> bool {
        self.has_scope(PRESENCE_WRITE_SCOPE)
    }

    pub fn can_read_presence(&self) -> bool {
        self.has_scope("Presence.Read.All") || self.can_write_presence()
    }

    /// Whether the token we request can enumerate teams and channels.
    pub fn can_read_teams(&self) -> bool {
        self.has_scope(TEAMS_READ_SCOPE)
    }

    fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s.eq_ignore_ascii_case(scope))
    }

    pub fn devicecode_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
            self.tenant_id
        )
    }

    pub fn token_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        )
    }

    /// URL the TUI points its subscriptions at (`None` if no tunnel configured).
    pub fn notification_url(&self) -> Option<String> {
        self.tunnel_base_url
            .as_ref()
            .map(|b| format!("{b}/notifications"))
    }

    pub fn lifecycle_url(&self) -> Option<String> {
        self.tunnel_base_url
            .as_ref()
            .map(|b| format!("{b}/lifecycle"))
    }
}

/// True for `1`, `true`, `yes`, `on` (case-insensitive).
fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

fn env_required(key: &str) -> Result<String> {
    std::env::var(key)
        .with_context(|| format!("required environment variable {key} is not set"))
        .map(|s| s.trim().to_string())
        .and_then(|s| {
            if s.is_empty() {
                anyhow::bail!("environment variable {key} is empty");
            }
            Ok(s)
        })
}

fn default_cache_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "AskArk", "ask-ark")
        .context("could not determine a config directory for this platform")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tunnel: Option<&str>) -> Config {
        Config {
            client_id: "cid".into(),
            tenant_id: "organizations".into(),
            scopes: vec!["User.Read".into(), "Mail.Send".into()],
            tunnel_base_url: tunnel.map(|s| s.to_string()),
            redis_url: "redis://127.0.0.1:6379".into(),
            token_cache_path: PathBuf::from("/tmp/x.json"),
            token_refresh_command: None,
            client_state: "secret".into(),
            notifications: true,
        }
    }

    #[test]
    fn builds_endpoints_and_urls() {
        let c = sample(Some("https://m365.example.com"));
        assert_eq!(c.scope_string(), "User.Read Mail.Send");
        assert!(!c.can_read_presence());
        assert_eq!(
            c.token_endpoint(),
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
        );
        assert_eq!(
            c.notification_url().as_deref(),
            Some("https://m365.example.com/notifications")
        );
        assert_eq!(
            c.lifecycle_url().as_deref(),
            Some("https://m365.example.com/lifecycle")
        );
    }

    #[test]
    fn graph_base_defaults_to_the_real_endpoint() {
        // Serialised with the override test below: both touch process env.
        std::env::remove_var("M365_GRAPH_BASE");
        assert_eq!(graph_base(), GRAPH_BASE);

        std::env::set_var("M365_GRAPH_BASE", "http://127.0.0.1:8765/v1.0/");
        assert_eq!(graph_base(), "http://127.0.0.1:8765/v1.0");

        // Empty means "not set", so an unfilled .env entry can't break the app.
        std::env::set_var("M365_GRAPH_BASE", "  ");
        assert_eq!(graph_base(), GRAPH_BASE);
        std::env::remove_var("M365_GRAPH_BASE");
    }

    #[test]
    fn no_tunnel_means_no_notification_url() {
        let c = sample(None);
        assert!(c.notification_url().is_none());
    }
}
