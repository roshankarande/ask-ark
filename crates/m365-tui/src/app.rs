//! Application state and the async orchestration behind it.
//!
//! The UI thread never blocks on the network: key handlers spawn tokio tasks
//! that fetch from Graph and send an [`AppMessage`] back over an mpsc channel,
//! which the main loop applies to the state before the next redraw.

use std::future::Future;

use anyhow::Context;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use m365_core::events::{ChangeEvent, ChangeKind};
use m365_core::models::{
    Attachment, Chat, ChatMessage, Event as CalEvent, MailFolder, MailMessage, Presence, Team, User,
};
use m365_core::{calendar, channels, chats, mail, people, Session};
use ratatui::text::Text;
use tokio::sync::mpsc;

use crate::content;
use crate::editor::TextInput;
use crate::navigation;

/// Messages sent from background tasks to the UI loop.
#[derive(Debug)]
pub enum AppMessage {
    Status(String),
    Error(String),
    Whoami(User),
    Folders(Vec<MailFolder>),
    Messages {
        items: Vec<MailMessage>,
        next: Option<String>,
        mode: ListUpdate,
    },
    MessageBody(MailMessage),
    Calendar(Vec<CalEvent>),
    Chats(Vec<Chat>),
    ChatMessages {
        chat_id: String,
        messages: Vec<ChatMessage>,
        next: Option<String>,
        mode: ListUpdate,
    },
    Teams(Vec<Team>),
    Channels {
        team_id: String,
        channels: Vec<m365_core::models::Channel>,
    },
    ChannelMessages {
        team_id: String,
        channel_id: String,
        messages: Vec<ChatMessage>,
        next: Option<String>,
        mode: ListUpdate,
    },
    /// A send/action completed; optional status text and refresh hint.
    Done(String),
    /// Result of a cross-navigation request to open a chat by email.
    OpenChat(Option<String>),
    /// The signed-in user's presence (status), for the tab-bar indicator.
    /// `requested` is set when this followed an explicit change, so we can tell
    /// the user if the effective presence didn't match what they asked for.
    Presence {
        presence: Presence,
        requested: Option<String>,
    },
    /// Newest inbox messages, fetched purely to drive notifications.
    InboxPeek(Vec<MailMessage>),
    /// Attachments of the open mail message.
    Attachments {
        message_id: String,
        items: Vec<Attachment>,
    },
    /// A downloaded attachment, ready to write to disk.
    Downloaded {
        name: String,
        bytes: Vec<u8>,
    },
    /// Push-notification health, for the status bar.
    Push(PushState),
    /// Lightweight timer: refresh memory usage and expire stale status text.
    Tick,
    /// Periodic tick: refresh the current view from the server.
    Poll,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Outlook,
    Teams,
}

/// Which kind of reply the user asked for.
enum ReplyMode {
    Reply,
    ReplyAll,
    Forward,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutlookFocus {
    Folders,
    Messages,
    Reading,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TeamsMode {
    Chats,
    Channels,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TeamsFocus {
    List,
    Messages,
    Composer,
}

/// A transient full-screen/modal overlay.
pub enum Overlay {
    Help,
    Palette {
        query: String,
        sel: usize,
    },
    Search {
        query: String,
    },
    Compose(Compose),
    Calendar,
    /// Emoji reaction picker for the selected Teams message.
    React,
    /// Presence (status) picker for the signed-in user.
    Presence,
    /// Numbered links in the focused message, to open in a browser.
    Links,
    /// Attachments of the open mail message, to save to disk.
    Attachments,
}

/// Whether Graph push notifications are working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushState {
    /// No tunnel configured — polling only.
    Off,
    /// Tunnel configured, subscriptions being created.
    Connecting,
    /// Subscriptions live; changes arrive in seconds.
    Live,
    /// Graph rejected the subscriptions — still polling, but not instant.
    Failed(String),
}

/// Ticks (see `TICK_SECONDS` in main) a status message survives before being
/// cleared. Long enough to read, short enough not to linger.
const STATUS_TICKS_TO_LIVE: u32 = 5;

/// How many items to fetch per page — mail messages and Teams messages alike.
/// Scrolling to the end of a list pulls the next page of this size.
pub const PAGE_SIZE: u32 = 50;

/// How an incoming page of items updates the list already on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListUpdate {
    /// Fresh context (folder switch, conversation opened, search): replace all.
    Replace,
    /// Older items fetched by scrolling: append after what's loaded.
    Append,
    /// Periodic refresh: fold the newest page in without dropping the older
    /// pages the user has already scrolled back through.
    Merge,
}

/// Emoji reactions offered in the picker, keyed 1-7.
pub const REACTIONS: &[&str] = &["👍", "❤️", "😆", "😮", "😢", "😠", "🎉"];

/// A status the user can pick.
pub struct PresenceOption {
    pub label: &'static str,
    /// What `setUserPreferredPresence` records — the sticky preference a Teams
    /// client will surface.
    pub preferred: (&'static str, &'static str),
    /// What this app publishes as its own presence *session*, which is what
    /// makes the status visible with no Teams client running. `None` means no
    /// session, i.e. appear offline. The session API accepts a narrower
    /// vocabulary than the preference API, hence the remapping.
    pub session: Option<(&'static str, &'static str)>,
}

/// Presence options, keyed 1-6.
pub const PRESENCE_OPTIONS: &[PresenceOption] = &[
    PresenceOption {
        label: "Available",
        preferred: ("Available", "Available"),
        session: Some(("Available", "Available")),
    },
    PresenceOption {
        label: "Busy",
        // The session API only offers Busy/InACall or Busy/InAConferenceCall.
        preferred: ("Busy", "Busy"),
        session: Some(("Busy", "InACall")),
    },
    PresenceOption {
        label: "Do not disturb",
        preferred: ("DoNotDisturb", "DoNotDisturb"),
        session: Some(("DoNotDisturb", "Presenting")),
    },
    PresenceOption {
        label: "Be right back",
        // No BeRightBack in the session vocabulary; Away is the closest.
        preferred: ("BeRightBack", "BeRightBack"),
        session: Some(("Away", "Away")),
    },
    PresenceOption {
        label: "Away",
        preferred: ("Away", "Away"),
        session: Some(("Away", "Away")),
    },
    PresenceOption {
        label: "Appear offline",
        preferred: ("Offline", "OffWork"),
        session: None,
    },
];

/// How long each presence session is asserted for, and how often it is renewed.
/// Graph allows PT5M–PT4H; a one-hour lease with a 30-minute refresh keeps it
/// alive while the app runs without leaving a long stale status if it dies.
pub const PRESENCE_SESSION_LEASE: &str = "PT1H";
pub const PRESENCE_RENEW_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

#[allow(clippy::enum_variant_names)] // the "Mail" suffix distinguishes from Teams compose
pub enum ComposeKind {
    NewMail,
    ReplyMail { id: String },
    ReplyAllMail { id: String },
    ForwardMail { id: String },
}

impl ComposeKind {
    /// Which compose fields are shown/editable:
    /// 0 = To, 1 = Subject, 2 = Body, 3 = Attach (a file path to stage).
    pub fn fields(&self) -> &'static [usize] {
        match self {
            ComposeKind::NewMail => &[0, 1, 2, 3],
            ComposeKind::ReplyMail { .. } | ComposeKind::ReplyAllMail { .. } => &[2, 3],
            ComposeKind::ForwardMail { .. } => &[0, 2, 3],
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            ComposeKind::NewMail => "Compose mail",
            ComposeKind::ReplyMail { .. } => "Reply",
            ComposeKind::ReplyAllMail { .. } => "Reply all",
            ComposeKind::ForwardMail { .. } => "Forward",
        }
    }
}

pub struct Compose {
    pub kind: ComposeKind,
    pub to: TextInput,
    pub subject: TextInput,
    pub body: TextInput,
    /// Path being typed in the Attach field.
    pub attach: TextInput,
    /// Files staged for sending, as (path, size).
    pub attachments: Vec<(std::path::PathBuf, u64)>,
    /// 0 = To, 1 = Subject, 2 = Body, 3 = Attach.
    pub field: usize,
}

impl Compose {
    pub fn new(kind: ComposeKind, field: usize) -> Self {
        Self {
            kind,
            to: TextInput::new(),
            subject: TextInput::new(),
            body: TextInput::new(),
            attach: TextInput::new(),
            attachments: Vec::new(),
            field,
        }
    }

    /// Stage the path currently typed in the Attach field.
    /// Returns a message for the status line.
    fn stage_attachment(&mut self) -> String {
        let raw = self.attach.text();
        let raw = raw.trim();
        if raw.is_empty() {
            return String::new();
        }
        let path = expand_tilde(raw);
        match std::fs::metadata(&path) {
            Ok(md) if md.is_file() => {
                let size = md.len();
                self.attachments.push((path.clone(), size));
                self.attach.clear();
                format!("attached {} ({})", path.display(), human_size(size))
            }
            Ok(_) => format!("{} is not a file", path.display()),
            Err(e) => format!("cannot attach {}: {e}", path.display()),
        }
    }

    fn active_mut(&mut self) -> &mut TextInput {
        match self.field {
            0 => &mut self.to,
            1 => &mut self.subject,
            3 => &mut self.attach,
            _ => &mut self.body,
        }
    }
}

#[derive(Default)]
pub struct OutlookState {
    pub folders: Vec<MailFolder>,
    pub folder_sel: usize,
    pub messages: Vec<MailMessage>,
    pub msg_sel: usize,
    /// `@odata.nextLink` for the current folder listing (Some = more to load).
    pub messages_next: Option<String>,
    /// Guards against firing multiple "load more" requests at once.
    pub loading_more: bool,
    pub reading: Option<MailMessage>,
    /// Cached styled body of the open message (HTML parsed once, not per frame).
    pub reading_body: Option<Text<'static>>,
    /// Links referenced by the open message, numbered `[1]`, `[2]`, ...
    pub reading_links: Vec<String>,
    /// Attachments of the open message (fetched when it has any).
    pub reading_attachments: Vec<Attachment>,
    /// Line scroll offset of the reading pane.
    pub reading_scroll: u16,
    pub calendar: Vec<CalEvent>,
}

pub struct TeamsState {
    pub mode: TeamsMode,
    pub chats: Vec<Chat>,
    pub chat_sel: usize,
    pub teams: Vec<Team>,
    pub team_sel: usize,
    pub channels: Vec<m365_core::models::Channel>,
    pub channel_sel: usize,
    pub messages: Vec<ChatMessage>,
    /// Cached styled body per message, index-aligned with `messages`.
    pub messages_rendered: Vec<Text<'static>>,
    /// Links per message, index-aligned with `messages`.
    pub messages_links: Vec<Vec<String>>,
    /// Selected message index in the conversation pane (drives scroll + react).
    pub msg_sel: usize,
    /// `@odata.nextLink` for older messages in the open conversation.
    pub messages_next: Option<String>,
    /// Guards against firing multiple "load older" requests at once.
    pub loading_more: bool,
    /// Messages that arrived while the user was reading further back.
    pub unseen: usize,
    /// Index of the message being replied to, while composing a reply.
    pub replying_to: Option<usize>,
    pub open_chat_id: Option<String>,
    pub open_channel: Option<(String, String)>,
    pub composer: TextInput,
    pub focus: TeamsFocus,
}

impl Default for TeamsState {
    fn default() -> Self {
        Self {
            mode: TeamsMode::Chats,
            chats: Vec::new(),
            chat_sel: 0,
            teams: Vec::new(),
            team_sel: 0,
            channels: Vec::new(),
            channel_sel: 0,
            messages: Vec::new(),
            messages_rendered: Vec::new(),
            messages_links: Vec::new(),
            msg_sel: 0,
            messages_next: None,
            loading_more: false,
            unseen: 0,
            replying_to: None,
            open_chat_id: None,
            open_channel: None,
            composer: TextInput::new(),
            focus: TeamsFocus::List,
        }
    }
}

pub struct App {
    pub session: Session,
    pub tx: mpsc::Sender<AppMessage>,
    pub screen: Screen,
    pub outlook: OutlookState,
    pub outlook_focus: OutlookFocus,
    pub teams: TeamsState,
    pub overlay: Option<Overlay>,
    pub status: String,
    pub me: Option<User>,
    /// The user's current presence (shown in the tab bar).
    pub my_presence: Option<Presence>,
    /// Wall-clock of the last poll refresh (shown in the tab bar).
    pub last_sync: Option<String>,
    /// Whether instant push is working (shown in the status bar).
    pub push: PushState,
    /// Message ids already notified about, so a poll can't repeat them.
    notified: std::collections::HashSet<String>,
    /// Newest message id seen per chat, to spot arrivals in chats that aren't
    /// open. `None` until the first chat list lands — the first sync must not
    /// fire a burst of notifications for history.
    chat_seen: Option<std::collections::HashMap<String, String>>,
    /// Inbox message ids already seen, same baseline rule as `chat_seen`.
    mail_seen: Option<std::collections::HashSet<String>>,
    /// The presence session this app is currently publishing, and when it was
    /// last asserted — sessions expire, so they need renewing.
    pub presence_session: Option<(&'static str, &'static str)>,
    presence_session_at: Option<std::time::Instant>,
    /// Resident memory in KiB, refreshed on each tick.
    pub rss_kb: Option<u64>,
    /// Copy of `status` as of the last tick, plus how many ticks it has been
    /// unchanged — transient messages are cleared so nothing gets stuck.
    last_status: String,
    status_ticks: u32,
    /// Width the focused text area was last laid out at, so Up/Down move by the
    /// rows actually on screen. Set by the renderer, read by the key handler.
    pub text_width_hint: std::cell::Cell<usize>,
    /// Largest useful reading-pane scroll offset, set by the renderer once it
    /// knows the wrapped height of the open message.
    pub reading_max_scroll: std::cell::Cell<u16>,
    /// Borderless full-width view for clean terminal text selection.
    pub copy_mode: bool,
    pub copy_scroll: u16,
    pub should_quit: bool,
}

/// Palette command identifiers.
const PALETTE_COMMANDS: &[(&str, &str)] = &[
    ("outlook", "Switch to Outlook"),
    ("teams", "Switch to Teams"),
    ("compose", "Compose new mail"),
    ("calendar", "Open calendar (today)"),
    ("chat-sender", "Teams: chat with selected email's sender"),
    ("refresh", "Refresh current view"),
    ("help", "Show help"),
    ("quit", "Quit"),
];

impl App {
    pub fn new(session: Session, tx: mpsc::Sender<AppMessage>) -> Self {
        Self {
            session,
            tx,
            screen: Screen::Outlook,
            outlook: OutlookState::default(),
            outlook_focus: OutlookFocus::Messages,
            teams: TeamsState::default(),
            overlay: None,
            status: "loading…".into(),
            me: None,
            my_presence: None,
            last_sync: None,
            push: PushState::Off,
            notified: std::collections::HashSet::new(),
            chat_seen: None,
            mail_seen: None,
            presence_session: None,
            presence_session_at: None,
            rss_kb: read_rss_kb(),
            last_status: String::new(),
            status_ticks: 0,
            text_width_hint: std::cell::Cell::new(60),
            reading_max_scroll: std::cell::Cell::new(0),
            copy_mode: false,
            copy_scroll: 0,
            should_quit: false,
        }
    }

    /// Kick off the initial data loads.
    pub fn bootstrap(&mut self) {
        self.load_whoami();
        if self.session.config.can_read_presence() {
            self.load_presence();
        }
        self.load_folders();
        self.load_chats();
    }

    // -- background task helpers ------------------------------------------

    fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = anyhow::Result<AppMessage>> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let msg = match fut.await {
                Ok(m) => m,
                Err(e) => {
                    // Also record it: the status line is transient, the log isn't.
                    tracing::warn!("background task failed: {e:#}");
                    AppMessage::Error(format!("{e:#}"))
                }
            };
            let _ = tx.send(msg).await;
        });
    }

    fn load_whoami(&self) {
        let s = self.session.clone();
        self.spawn(async move { Ok(AppMessage::Whoami(s.whoami().await?)) });
    }

    fn load_presence(&self) {
        let s = self.session.clone();
        self.spawn(async move {
            Ok(AppMessage::Presence {
                presence: people::my_presence(&s.graph).await?,
                requested: None,
            })
        });
    }

    /// Apply a chosen status: record the sticky preference *and* publish this
    /// app as a presence session, which is what actually makes the status
    /// visible when no Teams client is running.
    fn set_presence(&mut self, opt: &'static PresenceOption) {
        self.presence_session = opt.session;
        self.presence_session_at = Some(std::time::Instant::now());

        let s = self.session.clone();
        let client_id = self.session.config.client_id.clone();
        let (pref_av, pref_act) = opt.preferred;
        let session = opt.session;
        let label = opt.label;
        self.spawn(async move {
            people::set_preferred_presence(&s.graph, pref_av, pref_act).await?;
            match session {
                Some((av, act)) => {
                    people::set_session_presence(
                        &s.graph,
                        &client_id,
                        av,
                        act,
                        PRESENCE_SESSION_LEASE,
                    )
                    .await?
                }
                // "Appear offline" means holding no session at all.
                None => {
                    let _ = people::clear_session_presence(&s.graph, &client_id).await;
                }
            }
            Ok(AppMessage::Presence {
                presence: people::my_presence(&s.graph).await?,
                requested: Some(label.to_string()),
            })
        });
    }

    /// Re-assert the presence session before it lapses.
    fn renew_presence_session(&mut self) {
        let (Some((av, act)), Some(at)) = (self.presence_session, self.presence_session_at) else {
            return;
        };
        if at.elapsed() < PRESENCE_RENEW_AFTER {
            return;
        }
        self.presence_session_at = Some(std::time::Instant::now());
        let s = self.session.clone();
        let client_id = self.session.config.client_id.clone();
        self.spawn(async move {
            people::set_session_presence(&s.graph, &client_id, av, act, PRESENCE_SESSION_LEASE)
                .await?;
            Ok(AppMessage::Status(String::new()))
        });
    }

    fn clear_presence(&mut self) {
        self.presence_session = None;
        self.presence_session_at = None;
        let s = self.session.clone();
        let client_id = self.session.config.client_id.clone();
        self.spawn(async move {
            people::clear_preferred_presence(&s.graph).await?;
            let _ = people::clear_session_presence(&s.graph, &client_id).await;
            Ok(AppMessage::Presence {
                presence: people::my_presence(&s.graph).await?,
                requested: None,
            })
        });
    }

    fn load_folders(&self) {
        let s = self.session.clone();
        self.spawn(async move { Ok(AppMessage::Folders(mail::list_folders(&s.graph).await?)) });
    }

    fn load_messages(&self, folder_id: String, mode: ListUpdate) {
        let s = self.session.clone();
        self.spawn(async move {
            let (items, next) = mail::list_messages(&s.graph, &folder_id, PAGE_SIZE).await?;
            Ok(AppMessage::Messages { items, next, mode })
        });
    }

    fn load_more_messages(&mut self) {
        if self.outlook.loading_more {
            return;
        }
        let Some(next_link) = self.outlook.messages_next.clone() else {
            return;
        };
        self.outlook.loading_more = true;
        self.status = "loading more…".into();
        let s = self.session.clone();
        self.spawn(async move {
            let (items, next) = mail::list_messages_more(&s.graph, &next_link).await?;
            Ok(AppMessage::Messages {
                items,
                next,
                mode: ListUpdate::Append,
            })
        });
    }

    /// Fetch the newest inbox messages solely to drive notifications. Runs
    /// regardless of which folder is on screen, so mail is announced even while
    /// reading elsewhere.
    fn peek_inbox(&self) {
        if !self.session.config.notifications {
            return;
        }
        let s = self.session.clone();
        self.spawn(async move {
            let (items, _) = mail::list_messages(&s.graph, "inbox", 15).await?;
            Ok(AppMessage::InboxPeek(items))
        });
    }

    fn load_attachments(&self, message_id: String) {
        let s = self.session.clone();
        self.spawn(async move {
            let items = mail::list_attachments(&s.graph, &message_id).await?;
            Ok(AppMessage::Attachments { message_id, items })
        });
    }

    /// Download one attachment; the write to disk happens on the UI thread.
    fn download_attachment(&mut self, index: usize) {
        let (Some(msg), Some(att)) = (
            self.outlook.reading.as_ref(),
            self.outlook.reading_attachments.get(index),
        ) else {
            return;
        };
        if !att.is_file() {
            self.status = "only file attachments can be saved".into();
            return;
        }
        let message_id = msg.id.clone();
        let attachment_id = att.id.clone();
        let name = att.display_name();
        self.status = format!("downloading {name}…");
        let s = self.session.clone();
        self.spawn(async move {
            let bytes = mail::download_attachment(&s.graph, &message_id, &attachment_id).await?;
            Ok(AppMessage::Downloaded { name, bytes })
        });
    }

    fn load_body(&self, id: String) {
        let s = self.session.clone();
        self.spawn(async move {
            Ok(AppMessage::MessageBody(
                mail::get_message(&s.graph, &id).await?,
            ))
        });
    }

    fn load_calendar(&self) {
        let s = self.session.clone();
        // Today .. +7 days in UTC.
        let start = chrono::Utc::now();
        let end = start + chrono::Duration::days(7);
        let start = start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let end = end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        self.spawn(async move {
            Ok(AppMessage::Calendar(
                calendar::calendar_view(&s.graph, &start, &end).await?,
            ))
        });
    }

    fn load_chats(&self) {
        let s = self.session.clone();
        self.spawn(async move { Ok(AppMessage::Chats(chats::list_chats(&s.graph, 40).await?)) });
    }

    fn load_chat_messages(&self, chat_id: String, mode: ListUpdate) {
        let s = self.session.clone();
        self.spawn(async move {
            let (messages, next) = chats::list_messages(&s.graph, &chat_id, PAGE_SIZE).await?;
            Ok(AppMessage::ChatMessages {
                chat_id,
                messages,
                next,
                mode,
            })
        });
    }

    /// Fetch the next page of older messages for the open conversation.
    fn load_more_teams_messages(&mut self) {
        if self.teams.loading_more {
            return;
        }
        let Some(next_link) = self.teams.messages_next.clone() else {
            return; // reached the start of the conversation
        };
        self.teams.loading_more = true;
        self.status = "loading older messages…".into();
        let s = self.session.clone();

        if let Some(chat_id) = self.teams.open_chat_id.clone() {
            self.spawn(async move {
                let (messages, next) = chats::list_messages_more(&s.graph, &next_link).await?;
                Ok(AppMessage::ChatMessages {
                    chat_id,
                    messages,
                    next,
                    mode: ListUpdate::Append,
                })
            });
        } else if let Some((team_id, channel_id)) = self.teams.open_channel.clone() {
            self.spawn(async move {
                let (messages, next) = channels::list_messages_more(&s.graph, &next_link).await?;
                Ok(AppMessage::ChannelMessages {
                    team_id,
                    channel_id,
                    messages,
                    next,
                    mode: ListUpdate::Append,
                })
            });
        } else {
            self.teams.loading_more = false;
        }
    }

    fn load_teams(&self) {
        let s = self.session.clone();
        self.spawn(async move { Ok(AppMessage::Teams(channels::joined_teams(&s.graph).await?)) });
    }

    fn load_channels(&self, team_id: String) {
        let s = self.session.clone();
        self.spawn(async move {
            let channels = channels::list_channels(&s.graph, &team_id).await?;
            Ok(AppMessage::Channels { team_id, channels })
        });
    }

    fn load_channel_messages(&self, team_id: String, channel_id: String, mode: ListUpdate) {
        let s = self.session.clone();
        self.spawn(async move {
            let (messages, next) =
                channels::list_messages(&s.graph, &team_id, &channel_id, PAGE_SIZE).await?;
            Ok(AppMessage::ChannelMessages {
                team_id,
                channel_id,
                messages,
                next,
                mode,
            })
        });
    }

    fn send_chat_message(&self, chat_id: String, text: String) {
        let s = self.session.clone();
        self.spawn(async move {
            chats::send_message(&s.graph, &chat_id, &text).await?;
            Ok(AppMessage::Done("message sent".into()))
        });
    }

    fn send_channel_message(&self, team_id: String, channel_id: String, text: String) {
        let s = self.session.clone();
        self.spawn(async move {
            channels::send_message(&s.graph, &team_id, &channel_id, &text).await?;
            Ok(AppMessage::Done("message posted".into()))
        });
    }

    fn open_chat_with_email(&self, email: String) {
        let s = self.session.clone();
        let me_id = self.me.as_ref().map(|m| m.id.clone());
        self.spawn(async move {
            let Some(me_id) = me_id else {
                return Ok(AppMessage::Error("still loading your profile".into()));
            };
            let id = navigation::chat_id_for_email(&s, &me_id, &email).await?;
            Ok(AppMessage::OpenChat(id))
        });
    }

    // -- applying background results --------------------------------------

    pub fn apply(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::Status(s) => self.status = s,
            AppMessage::Error(e) => {
                self.status = format!("error: {}", m365_core::util::graph_error_summary(&e));
            }
            AppMessage::Whoami(u) => {
                self.status = format!(
                    "signed in as {} <{}>",
                    u.display_name.clone().unwrap_or_default(),
                    u.best_email().unwrap_or("")
                );
                self.me = Some(u);
            }
            AppMessage::Folders(f) => {
                let first_load = self.outlook.folders.is_empty();
                self.outlook.folders = f;
                if first_load {
                    // Prefer Inbox as the initial selection, then load it.
                    if let Some(i) = self
                        .outlook
                        .folders
                        .iter()
                        .position(|f| f.display_name.as_deref() == Some("Inbox"))
                    {
                        self.outlook.folder_sel = i;
                    }
                    if let Some(f) = self.outlook.folders.get(self.outlook.folder_sel) {
                        self.load_messages(f.id.clone(), ListUpdate::Replace);
                    }
                } else {
                    // Refresh (poll): keep the user's selection, just clamp it.
                    self.outlook.folder_sel = self
                        .outlook
                        .folder_sel
                        .min(self.outlook.folders.len().saturating_sub(1));
                }
            }
            AppMessage::Messages { items, next, mode } => {
                self.outlook.loading_more = false;
                let selected_id = self
                    .outlook
                    .messages
                    .get(self.outlook.msg_sel)
                    .map(|m| m.id.clone());

                match mode {
                    ListUpdate::Replace => {
                        self.outlook.messages = items;
                        self.outlook.messages_next = next;
                    }
                    ListUpdate::Append => {
                        self.outlook.messages.extend(items);
                        self.outlook.messages_next = next;
                        self.status = if self.outlook.messages_next.is_some() {
                            format!("{} loaded (more available)", self.outlook.messages.len())
                        } else {
                            format!("{} loaded (all)", self.outlook.messages.len())
                        };
                    }
                    ListUpdate::Merge => {
                        // Refresh the newest window without discarding the older
                        // pages already loaded; `messages_next` is left alone as
                        // it already points past everything we hold.
                        let existing = std::mem::take(&mut self.outlook.messages);
                        self.outlook.messages =
                            merge_newest_first(items, existing, |m| m.id.clone());
                    }
                }

                self.outlook.msg_sel = selected_id
                    .and_then(|id| self.outlook.messages.iter().position(|m| m.id == id))
                    .unwrap_or(self.outlook.msg_sel)
                    .min(self.outlook.messages.len().saturating_sub(1));
            }
            AppMessage::MessageBody(m) => {
                let (ct, raw) = match &m.body {
                    Some(b) => (
                        b.content_type.as_deref(),
                        b.content.clone().unwrap_or_default(),
                    ),
                    None => (Some("text"), m.body_preview.clone().unwrap_or_default()),
                };
                let rendered = content::render_body(ct, &raw);
                self.outlook.reading_links = rendered.links;
                self.outlook.reading_body = Some(rendered.text);
                self.outlook.reading_attachments.clear();
                self.outlook.reading_scroll = 0;
                if m.has_attachments.unwrap_or(false) {
                    self.load_attachments(m.id.clone());
                }
                self.outlook.reading = Some(m);
            }
            AppMessage::Calendar(e) => self.outlook.calendar = e,
            AppMessage::Chats(c) => {
                self.notify_for_chats(&c);
                self.teams.chats = c;
                self.teams.chat_sel = self
                    .teams
                    .chat_sel
                    .min(self.teams.chats.len().saturating_sub(1));
            }
            AppMessage::ChatMessages {
                chat_id,
                messages,
                next,
                mode,
            } => {
                if self.teams.open_chat_id.as_deref() == Some(&chat_id) {
                    self.set_teams_messages(messages, next, mode);
                }
            }
            AppMessage::Teams(t) => self.teams.teams = t,
            AppMessage::Channels { team_id, channels } => {
                if self.teams.teams.get(self.teams.team_sel).map(|t| &t.id) == Some(&team_id) {
                    self.teams.channels = channels;
                    self.teams.channel_sel = 0;
                }
            }
            AppMessage::ChannelMessages {
                team_id,
                channel_id,
                messages,
                next,
                mode,
            } => {
                if self.teams.open_channel.as_ref() == Some(&(team_id, channel_id)) {
                    self.set_teams_messages(messages, next, mode);
                }
            }
            AppMessage::Done(s) => {
                self.status = s;
                self.refresh_current();
            }
            AppMessage::OpenChat(Some(id)) => {
                self.screen = Screen::Teams;
                self.teams.mode = TeamsMode::Chats;
                self.teams.open_chat_id = Some(id.clone());
                self.teams.focus = TeamsFocus::Messages;
                self.load_chat_messages(id, ListUpdate::Replace);
                self.load_chats();
            }
            AppMessage::OpenChat(None) => {
                self.status = "that sender isn't a Teams user in your directory".into();
            }
            AppMessage::Presence {
                presence,
                requested,
            } => {
                let effective = presence.availability.clone().unwrap_or_default();
                self.status = match requested {
                    Some(req) if effective.eq_ignore_ascii_case(&req) => {
                        format!("presence set to {req}")
                    }
                    // e.g. requested Available, Teams reports AvailableIdle.
                    Some(req) if effective.to_lowercase().starts_with(&req.to_lowercase()) => {
                        format!("presence set to {req} (Teams reports {effective})")
                    }
                    // Chose "Appear offline": Offline is the expected outcome.
                    Some(req) if effective.eq_ignore_ascii_case("Offline") => {
                        format!("presence set to {req} (you appear Offline)")
                    }
                    // Graph settled on something else — usually a Teams client
                    // that is running and idle, which outranks our session.
                    Some(req) => format!(
                        "presence set to {req}; Teams currently shows '{effective}' (a signed-in Teams client can override this)"
                    ),
                    None => format!("presence: {effective}"),
                };
                self.my_presence = Some(presence);
            }
            AppMessage::InboxPeek(items) => self.notify_for_mail(&items),
            AppMessage::Attachments { message_id, items } => {
                // Ignore a late response for a message we've navigated away from.
                if self.outlook.reading.as_ref().map(|m| m.id.as_str()) == Some(&message_id) {
                    // Inline images (signatures, logos) aren't useful downloads.
                    self.outlook.reading_attachments = items
                        .into_iter()
                        .filter(|a| !a.is_inline.unwrap_or(false))
                        .collect();
                }
            }
            AppMessage::Downloaded { name, bytes } => {
                let size = bytes.len();
                match crate::files::save(&name, &bytes) {
                    Ok(path) => {
                        self.status = format!("saved {} ({size} bytes) to {}", name, path.display())
                    }
                    Err(e) => self.status = format!("could not save {name}: {e:#}"),
                }
            }
            AppMessage::Push(state) => {
                // Surface a broken tunnel instead of silently falling back.
                if let PushState::Failed(reason) = &state {
                    self.status = format!("push unavailable, using 20s polling — {reason}");
                }
                self.push = state;
            }
            AppMessage::Tick => {
                self.rss_kb = read_rss_kb();
                // Clear a message once it has sat unchanged for a while, so the
                // status bar never shows something from half an hour ago.
                if self.status.is_empty() {
                    self.status_ticks = 0;
                } else if self.status == self.last_status {
                    self.status_ticks += 1;
                    if self.status_ticks >= STATUS_TICKS_TO_LIVE {
                        self.status.clear();
                        self.status_ticks = 0;
                    }
                } else {
                    self.last_status = self.status.clone();
                    self.status_ticks = 0;
                }
            }
            AppMessage::Poll => self.poll(),
        }
    }

    /// Refresh the current view from the server. Driven by a periodic timer so
    /// the UI stays live even without the push tunnel.
    fn poll(&mut self) {
        self.last_sync = Some(now_hms());
        self.renew_presence_session();
        self.peek_inbox();
        // Mail: refresh folder unread counts + the open folder's messages.
        self.load_folders();
        if let Some(f) = self.outlook.folders.get(self.outlook.folder_sel) {
            self.load_messages(f.id.clone(), ListUpdate::Merge);
        }
        // Teams: refresh chat list + whichever conversation is open.
        self.load_chats();
        if let Some(id) = self.teams.open_chat_id.clone() {
            self.load_chat_messages(id, ListUpdate::Merge);
        }
        if let Some((t, c)) = self.teams.open_channel.clone() {
            self.load_channel_messages(t, c, ListUpdate::Merge);
        }
    }

    pub fn on_change(&mut self, change: ChangeEvent) {
        match change.kind() {
            ChangeKind::Mail => {
                self.status = "📬 new mail".into();
                self.peek_inbox();
                if let Some(f) = self.outlook.folders.get(self.outlook.folder_sel) {
                    self.load_messages(f.id.clone(), ListUpdate::Merge);
                }
            }
            ChangeKind::Chat => {
                self.status = "💬 chat update".into();
                self.load_chats();
                if let Some(id) = self.teams.open_chat_id.clone() {
                    self.load_chat_messages(id, ListUpdate::Merge);
                }
            }
            ChangeKind::Channel => {
                if let Some((t, c)) = self.teams.open_channel.clone() {
                    self.load_channel_messages(t, c, ListUpdate::Merge);
                }
            }
            ChangeKind::Other => {}
        }
    }

    /// Set the Teams conversation messages and pre-render their Markdown once
    /// (HTML→md is the expensive part; keep it off the render path).
    fn set_teams_messages(
        &mut self,
        messages: Vec<ChatMessage>,
        next: Option<String>,
        mode: ListUpdate,
    ) {
        self.teams.loading_more = false;
        let mut follow_newest = false;
        // Remember what was selected so a refresh doesn't move the cursor to a
        // different message when others arrive.
        let selected_id = self
            .teams
            .messages
            .get(self.teams.msg_sel)
            .map(|m| m.id.clone());

        // Graph answers newest-first; the conversation reads oldest-first, so
        // the newest message sits at the bottom next to the composer.
        let mut page = messages;
        page.reverse();

        match mode {
            ListUpdate::Replace => {
                self.teams.messages = page;
                self.teams.messages_next = next;
            }
            ListUpdate::Append => {
                // An older page belongs *before* everything already loaded.
                let existing = std::mem::take(&mut self.teams.messages);
                self.teams.messages = page;
                self.teams.messages.extend(existing);
                self.teams.messages_next = next;
                self.status = if self.teams.messages_next.is_some() {
                    format!("{} messages loaded (more above)", self.teams.messages.len())
                } else {
                    format!(
                        "{} messages loaded (start of conversation)",
                        self.teams.messages.len()
                    )
                };
            }
            ListUpdate::Merge => {
                // Chat convention: if the user is sitting on the newest message,
                // follow new arrivals; if they've scrolled back to read history,
                // hold their place and just count what came in.
                follow_newest = self.teams.msg_sel + 1 >= self.teams.messages.len();
                let known: std::collections::HashSet<&str> =
                    self.teams.messages.iter().map(|m| m.id.as_str()).collect();
                let arrived = page
                    .iter()
                    .filter(|m| !known.contains(m.id.as_str()))
                    .count();
                if !follow_newest {
                    self.teams.unseen += arrived;
                }

                // The fetched page supersedes the newest window; everything
                // older that the user has scrolled back through is kept.
                let fresh: std::collections::HashSet<String> =
                    page.iter().map(|m| m.id.clone()).collect();
                let mut kept: Vec<ChatMessage> = std::mem::take(&mut self.teams.messages)
                    .into_iter()
                    .filter(|m| !fresh.contains(&m.id))
                    .collect();
                kept.extend(page);
                self.teams.messages = kept;
            }
        }

        // Order the list ourselves rather than trusting the order pages arrive
        // in: Graph's default ordering is not strictly by creation time, and
        // merging pages can interleave. Sorting by timestamp (id breaks ties —
        // Teams ids are epoch milliseconds) is cheap and always correct.
        self.teams.messages.sort_by_key(sort_key);

        // Re-render bodies for whatever the list now holds (HTML parse is cached
        // per message, so this only costs on changed content).
        let rendered: Vec<_> = self
            .teams
            .messages
            .iter()
            .map(|m| {
                let ct = m.body.as_ref().and_then(|b| b.content_type.as_deref());
                content::render_body(ct, &m.text())
            })
            .collect();
        // Shared files are openable too: fold their URLs into the link list.
        self.teams.messages_links = self
            .teams
            .messages
            .iter()
            .zip(rendered.iter())
            .map(|(m, r)| {
                let mut links = r.links.clone();
                for att in &m.attachments {
                    if let Some(url) = att.content_url.as_ref().filter(|u| !u.is_empty()) {
                        if !links.contains(url) {
                            links.push(url.clone());
                        }
                    }
                }
                links
            })
            .collect();
        self.teams.messages_rendered = rendered.into_iter().map(|r| r.text).collect();

        let last = self.teams.messages.len().saturating_sub(1);
        self.teams.msg_sel = match mode {
            // Opening a conversation lands on the newest message, at the bottom.
            ListUpdate::Replace => last,
            _ if follow_newest => {
                self.teams.unseen = 0;
                last
            }
            // Otherwise stay on the same message, wherever it moved to.
            _ => selected_id
                .and_then(|id| self.teams.messages.iter().position(|m| m.id == id))
                .unwrap_or(self.teams.msg_sel)
                .min(last),
        };
    }

    fn refresh_current(&mut self) {
        match self.screen {
            Screen::Outlook => {
                if let Some(f) = self.outlook.folders.get(self.outlook.folder_sel) {
                    self.load_messages(f.id.clone(), ListUpdate::Merge);
                }
            }
            Screen::Teams => match self.teams.mode {
                TeamsMode::Chats => {
                    self.load_chats();
                    if let Some(id) = self.teams.open_chat_id.clone() {
                        self.load_chat_messages(id, ListUpdate::Merge);
                    }
                }
                TeamsMode::Channels => {
                    if let Some((t, c)) = self.teams.open_channel.clone() {
                        self.load_channel_messages(t, c, ListUpdate::Merge);
                    }
                }
            },
        }
    }

    /// Links of whichever message is in focus.
    pub fn focused_links(&self) -> &[String] {
        match self.screen {
            Screen::Outlook => &self.outlook.reading_links,
            Screen::Teams => self
                .teams
                .messages_links
                .get(self.teams.msg_sel)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        }
    }

    fn open_link(&mut self, index: usize) {
        let Some(url) = self.focused_links().get(index).cloned() else {
            return;
        };
        match crate::opener::open_url(&url) {
            Ok(()) => self.status = format!("opening {}", truncate_url(&url)),
            Err(e) => self.status = format!("could not open link: {e:#}"),
        }
    }

    /// Raise notifications for chats whose newest message changed: direct
    /// messages always, group chats only when they `@mention` the user.
    fn notify_for_chats(&mut self, chats: &[Chat]) {
        if !self.session.config.notifications {
            return;
        }
        let my_id = self.me.as_ref().map(|m| m.id.clone());
        let my_name = self.me.as_ref().and_then(|m| m.display_name.clone());

        // First sync only records the baseline; notifying here would announce
        // every conversation's history at startup.
        let Some(seen) = self.chat_seen.as_mut() else {
            self.chat_seen = Some(
                chats
                    .iter()
                    .filter_map(|c| {
                        let id = c.last_message_preview.as_ref()?.id.clone()?;
                        Some((c.id.clone(), id))
                    })
                    .collect(),
            );
            return;
        };

        let mut to_notify: Vec<(String, String)> = Vec::new();
        for chat in chats {
            let Some(preview) = chat.last_message_preview.as_ref() else {
                continue;
            };
            let Some(msg_id) = preview.id.clone() else {
                continue;
            };
            let previous = seen.insert(chat.id.clone(), msg_id.clone());
            if previous.as_deref() == Some(msg_id.as_str()) {
                continue; // nothing new here
            }

            // Never notify for our own messages, or twice for the same one.
            let from_id = preview
                .from
                .as_ref()
                .and_then(|f| f.user.as_ref())
                .and_then(|u| u.id.as_deref());
            if from_id.is_some() && from_id == my_id.as_deref() {
                continue;
            }
            if !self.notified.insert(msg_id.clone()) {
                continue;
            }

            let body = preview
                .body
                .as_ref()
                .and_then(|b| b.content.clone())
                .unwrap_or_default();
            let mentioned =
                crate::notify::mentions_me(&[], &body, my_id.as_deref(), my_name.as_deref());
            if !crate::notify::should_notify(chat.chat_type.as_deref(), mentioned) {
                continue;
            }

            let who = preview
                .from
                .as_ref()
                .and_then(|f| f.user.as_ref())
                .and_then(|u| u.display_name.clone())
                .unwrap_or_else(|| chat.label(my_id.as_deref()));
            let title = if mentioned {
                format!("{who} mentioned you")
            } else {
                who
            };
            to_notify.push((
                title,
                content::plain(&content::render_body(None, &body).text),
            ));
        }

        // Keep the id set from growing without bound over a long session.
        if self.notified.len() > 1000 {
            self.notified.clear();
        }
        for (title, body) in to_notify {
            crate::notify::send(&title, &body);
        }
    }

    /// Announce new, still-unread inbox mail.
    fn notify_for_mail(&mut self, items: &[MailMessage]) {
        if !self.session.config.notifications {
            return;
        }
        // First sync records a baseline; otherwise the whole inbox would
        // announce itself on startup.
        let Some(seen) = self.mail_seen.as_mut() else {
            self.mail_seen = Some(items.iter().map(|m| m.id.clone()).collect());
            return;
        };

        let mut to_notify: Vec<(String, String)> = Vec::new();
        for m in items {
            if !seen.insert(m.id.clone()) {
                continue; // already known
            }
            // Read elsewhere (phone, Outlook) before we got here.
            if m.is_read.unwrap_or(false) {
                continue;
            }
            if !self.notified.insert(m.id.clone()) {
                continue;
            }
            to_notify.push((
                m.sender_name(),
                m.subject.clone().unwrap_or_else(|| "(no subject)".into()),
            ));
        }

        if seen.len() > 1000 {
            seen.clear();
        }
        if self.notified.len() > 1000 {
            self.notified.clear();
        }
        for (who, subject) in to_notify {
            crate::notify::send(&format!("✉ {who}"), &subject);
        }
    }

    // -- clipboard ---------------------------------------------------------

    /// Copy the focused item: the open email's body, or the selected Teams
    /// message's body.
    fn yank_current(&mut self) {
        let text = match self.screen {
            Screen::Outlook => self.outlook.reading_body.as_ref().map(content::plain),
            Screen::Teams => self
                .teams
                .messages_rendered
                .get(self.teams.msg_sel)
                .map(content::plain),
        };
        match text {
            Some(t) if !t.trim().is_empty() => self.copy_to_clipboard(&t, "message"),
            _ => self.status = "nothing to copy — open a message first".into(),
        }
    }

    /// Copy everything in the current view: the whole email (with headers) or
    /// the whole conversation.
    fn yank_all(&mut self) {
        let text = match self.screen {
            Screen::Outlook => crate::ui::email_lines(self)
                .map(|lines| lines_to_plain(&lines))
                .unwrap_or_default(),
            Screen::Teams => lines_to_plain(&crate::ui::conversation_lines(self, false).0),
        };
        if text.trim().is_empty() {
            self.status = "nothing to copy".into();
        } else {
            self.copy_to_clipboard(&text, "all");
        }
    }

    fn copy_to_clipboard(&mut self, text: &str, what: &str) {
        match crate::clipboard::copy(text) {
            Ok(via) => {
                let bytes = text.len();
                self.status = format!("copied {what} to clipboard ({bytes} bytes, via {via})");
            }
            Err(e) => self.status = format!("copy failed: {e:#}"),
        }
    }

    // -- key handling ------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        // Overlays capture input first.
        if self.overlay.is_some() {
            self.on_key_overlay(key);
            return;
        }

        // Copy mode swallows input so stray keys can't fire actions while the
        // user is selecting text.
        if self.copy_mode {
            match key.code {
                KeyCode::Char('z') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.copy_mode = false;
                }
                KeyCode::Char('y') => self.yank_all(),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.copy_scroll = self.copy_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.copy_scroll = self.copy_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.copy_scroll = self.copy_scroll.saturating_add(20),
                KeyCode::PageUp => self.copy_scroll = self.copy_scroll.saturating_sub(20),
                KeyCode::Char('g') => self.copy_scroll = 0,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        // While typing a Teams message, only Ctrl-modified/function keys act
        // globally — otherwise letters like q/p/z would be stolen from the text.
        let typing = self.screen == Screen::Teams && self.teams.focus == TeamsFocus::Composer;

        // Global bindings.
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return;
            }
            (KeyCode::Char('q'), KeyModifiers::NONE) if !typing => {
                self.should_quit = true;
                return;
            }
            (KeyCode::Char('z'), KeyModifiers::NONE) if !typing => {
                self.copy_mode = true;
                self.copy_scroll = 0;
                return;
            }
            (KeyCode::Char('y'), KeyModifiers::NONE) if !typing => {
                self.yank_current();
                return;
            }
            (KeyCode::Char('Y'), _) if !typing => {
                self.yank_all();
                return;
            }
            (KeyCode::F(2), _) => {
                self.screen = match self.screen {
                    Screen::Outlook => Screen::Teams,
                    Screen::Teams => Screen::Outlook,
                };
                return;
            }
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.overlay = Some(Overlay::Palette {
                    query: String::new(),
                    sel: 0,
                });
                return;
            }
            (KeyCode::Char('?'), _) if !typing => {
                self.overlay = Some(Overlay::Help);
                return;
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) if !typing => {
                self.overlay = Some(Overlay::Presence);
                return;
            }
            (KeyCode::Char('A'), _) if !typing => {
                if self.screen == Screen::Outlook {
                    if self.outlook.reading_attachments.is_empty() {
                        self.status = "no attachments on this message".into();
                    } else {
                        self.overlay = Some(Overlay::Attachments);
                    }
                }
                return;
            }
            (KeyCode::Char('o'), KeyModifiers::NONE) if !typing => {
                if self.focused_links().is_empty() {
                    self.status = "no links in this message".into();
                } else {
                    self.overlay = Some(Overlay::Links);
                }
                return;
            }
            _ => {}
        }

        match self.screen {
            Screen::Outlook => self.on_key_outlook(key),
            Screen::Teams => self.on_key_teams(key),
        }
    }

    fn on_key_outlook(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => {
                self.outlook_focus = match self.outlook_focus {
                    OutlookFocus::Folders => OutlookFocus::Messages,
                    OutlookFocus::Messages => OutlookFocus::Reading,
                    OutlookFocus::Reading => OutlookFocus::Folders,
                };
            }
            KeyCode::Char('g') => self.load_calendar_and_show(),
            KeyCode::Char('c') => {
                self.overlay = Some(Overlay::Compose(empty_compose()));
            }
            KeyCode::Char('/') => {
                self.overlay = Some(Overlay::Search {
                    query: String::new(),
                });
            }
            KeyCode::Char('r') => self.open_reply(ReplyMode::Reply),
            KeyCode::Char('a') => self.open_reply(ReplyMode::ReplyAll),
            KeyCode::Char('f') => self.open_reply(ReplyMode::Forward),
            KeyCode::Up | KeyCode::Char('k') => self.outlook_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.outlook_move(1),
            KeyCode::PageUp => self.outlook_move(-10),
            KeyCode::PageDown => self.outlook_move(10),
            // h/l move between panes: out to the left, into the thing on the
            // right. Esc is a synonym for backing out.
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.outlook_out(),
            KeyCode::Right | KeyCode::Char('l') => self.outlook_into(),
            KeyCode::Home if self.outlook_focus == OutlookFocus::Reading => {
                self.outlook.reading_scroll = 0;
            }
            KeyCode::End if self.outlook_focus == OutlookFocus::Reading => {
                self.outlook.reading_scroll = self.reading_max_scroll.get();
            }
            KeyCode::Enter => self.outlook_enter(),
            _ => {}
        }
    }

    fn load_calendar_and_show(&mut self) {
        self.load_calendar();
        self.overlay = Some(Overlay::Calendar);
    }

    fn outlook_move(&mut self, delta: i32) {
        match self.outlook_focus {
            OutlookFocus::Folders => {
                self.outlook.folder_sel =
                    step(self.outlook.folder_sel, delta, self.outlook.folders.len());
            }
            // In the reading pane, j/k scroll the message body.
            OutlookFocus::Reading => {
                let max = self.reading_max_scroll.get();
                self.outlook.reading_scroll = if delta > 0 {
                    self.outlook
                        .reading_scroll
                        .saturating_add(delta as u16)
                        .min(max)
                } else {
                    self.outlook
                        .reading_scroll
                        .saturating_sub(delta.unsigned_abs() as u16)
                };
            }
            OutlookFocus::Messages => {
                let len = self.outlook.messages.len();
                self.outlook.msg_sel = step(self.outlook.msg_sel, delta, len);
                // Scrolling down onto the last row pulls the next page.
                if delta > 0 && len > 0 && self.outlook.msg_sel == len - 1 {
                    self.load_more_messages();
                }
            }
        }
    }

    /// Move focus one pane left: Reading → Messages → Folders.
    fn outlook_out(&mut self) {
        self.outlook_focus = match self.outlook_focus {
            OutlookFocus::Reading => OutlookFocus::Messages,
            OutlookFocus::Messages | OutlookFocus::Folders => OutlookFocus::Folders,
        };
    }

    /// Move focus one pane right, opening whatever is selected on the way — the
    /// same thing Enter does.
    fn outlook_into(&mut self) {
        if self.outlook_focus != OutlookFocus::Reading {
            self.outlook_enter();
        }
    }

    fn outlook_enter(&mut self) {
        match self.outlook_focus {
            OutlookFocus::Folders => {
                if let Some(f) = self.outlook.folders.get(self.outlook.folder_sel) {
                    self.outlook.msg_sel = 0;
                    self.load_messages(f.id.clone(), ListUpdate::Replace);
                    self.outlook_focus = OutlookFocus::Messages;
                }
            }
            OutlookFocus::Messages | OutlookFocus::Reading => {
                if let Some(m) = self.current_mail() {
                    let id = m.id.clone();
                    self.load_body(id);
                    self.outlook_focus = OutlookFocus::Reading;
                }
            }
        }
    }

    fn current_mail(&self) -> Option<&MailMessage> {
        self.outlook.messages.get(self.outlook.msg_sel)
    }

    fn open_reply(&mut self, mode: ReplyMode) {
        let Some(m) = self.current_mail() else {
            self.status = "select a message first".into();
            return;
        };
        let id = m.id.clone();
        let (kind, field) = match mode {
            ReplyMode::Reply => (ComposeKind::ReplyMail { id }, 2),
            ReplyMode::ReplyAll => (ComposeKind::ReplyAllMail { id }, 2),
            ReplyMode::Forward => (ComposeKind::ForwardMail { id }, 0),
        };
        self.overlay = Some(Overlay::Compose(Compose::new(kind, field)));
    }

    fn on_key_teams(&mut self, key: KeyEvent) {
        // Composer captures typing when focused.
        if self.teams.focus == TeamsFocus::Composer {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let input = &mut self.teams.composer;
            match key.code {
                KeyCode::Esc => {
                    self.teams.replying_to = None;
                    self.teams.focus = TeamsFocus::Messages;
                }
                KeyCode::Tab => self.teams.focus = TeamsFocus::List,
                // Enter sends; Shift/Alt+Enter inserts a newline instead.
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    input.insert('\n')
                }
                KeyCode::Enter => self.teams_send(),
                KeyCode::Up => input.move_row(-1, self.text_width_hint.get()),
                KeyCode::Down => input.move_row(1, self.text_width_hint.get()),
                KeyCode::Backspace => input.backspace(),
                KeyCode::Delete => input.delete(),
                KeyCode::Char('w') if ctrl => input.delete_word_before(),
                KeyCode::Char('u') if ctrl => input.delete_to_line_start(),
                KeyCode::Char('k') if ctrl => input.delete_to_line_end(),
                KeyCode::Left if ctrl => input.word_left(),
                KeyCode::Right if ctrl => input.word_right(),
                KeyCode::Left => input.left(),
                KeyCode::Right => input.right(),
                KeyCode::Home if ctrl => input.start_of_text(),
                KeyCode::End if ctrl => input.end_of_text(),
                KeyCode::Char('a') if ctrl => input.home(),
                KeyCode::Char('e') if ctrl => input.end(),
                KeyCode::Home => input.home(),
                KeyCode::End => input.end(),
                KeyCode::Char(c) if !ctrl => input.insert(c),
                _ => {}
            }
            return;
        }

        match key.code {
            // Back out to the conversation list from the messages pane.
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => {
                self.teams.focus = TeamsFocus::List;
            }
            // ...and into the conversation from the list, opening it.
            KeyCode::Right | KeyCode::Char('l') if self.teams.focus == TeamsFocus::List => {
                self.teams_enter();
            }
            KeyCode::Tab => {
                self.teams.focus = match self.teams.focus {
                    TeamsFocus::List => TeamsFocus::Messages,
                    TeamsFocus::Messages => TeamsFocus::Composer,
                    TeamsFocus::Composer => TeamsFocus::List,
                };
            }
            KeyCode::Char('t') => {
                // Toggle chats/channels mode. Listing teams needs a scope that
                // chats don't, so say so plainly instead of letting Graph 403.
                self.teams.mode = match self.teams.mode {
                    TeamsMode::Chats => {
                        if !self.session.config.can_read_teams() {
                            self.status = "channels need the Team.ReadBasic.All permission — grant it, then set M365_TEAMS_CHANNELS=1 and sign in again".into();
                            return;
                        }
                        if self.teams.teams.is_empty() {
                            self.load_teams();
                        }
                        TeamsMode::Channels
                    }
                    TeamsMode::Channels => TeamsMode::Chats,
                };
            }
            KeyCode::Char('i') => self.teams.focus = TeamsFocus::Composer,
            // Reply to the selected message: same composer, quoted on send.
            KeyCode::Char('r')
                if self.teams.focus == TeamsFocus::Messages && !self.teams.messages.is_empty() =>
            {
                self.teams.replying_to = Some(self.teams.msg_sel);
                self.teams.focus = TeamsFocus::Composer;
            }
            KeyCode::Char('e')
                if self.teams.focus == TeamsFocus::Messages && !self.teams.messages.is_empty() =>
            {
                self.overlay = Some(Overlay::React);
            }
            // Jump back to the newest message and resume following it.
            KeyCode::End | KeyCode::Char('g') if self.teams.focus == TeamsFocus::Messages => {
                self.teams.msg_sel = self.teams.messages.len().saturating_sub(1);
                self.teams.unseen = 0;
            }
            KeyCode::Home if self.teams.focus == TeamsFocus::Messages => {
                self.teams.msg_sel = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.teams_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.teams_move(1),
            KeyCode::Enter => self.teams_enter(),
            _ => {}
        }
    }

    fn teams_move(&mut self, delta: i32) {
        // In the conversation pane, j/k move the selected message (skipping
        // deleted ones); the selection drives both scroll and reactions.
        if self.teams.focus == TeamsFocus::Messages {
            let n = self.teams.messages.len() as i32;
            if n == 0 {
                return;
            }
            let mut i = self.teams.msg_sel as i32;
            loop {
                i += delta.signum();
                if i < 0 {
                    // Past the oldest loaded message — pull the previous page.
                    self.load_more_teams_messages();
                    return;
                }
                if i >= n {
                    return; // already on the newest
                }
                if self.teams.messages[i as usize].deleted_date_time.is_none() {
                    self.teams.msg_sel = i as usize;
                    if self.teams.msg_sel + 1 >= self.teams.messages.len() {
                        self.teams.unseen = 0;
                        self.teams.replying_to = None; // caught up with the newest
                    }
                    // Prefetch when landing on the oldest, so scrolling further
                    // back doesn't stall.
                    if delta < 0 && self.teams.msg_sel == 0 {
                        self.load_more_teams_messages();
                    }
                    return;
                }
            }
        }
        match (self.teams.mode, self.teams.focus) {
            (TeamsMode::Chats, TeamsFocus::List) => {
                self.teams.chat_sel = step(self.teams.chat_sel, delta, self.teams.chats.len());
            }
            (TeamsMode::Channels, TeamsFocus::List) => {
                // Navigate channels; if none loaded, navigate teams.
                if self.teams.channels.is_empty() {
                    self.teams.team_sel = step(self.teams.team_sel, delta, self.teams.teams.len());
                } else {
                    self.teams.channel_sel =
                        step(self.teams.channel_sel, delta, self.teams.channels.len());
                }
            }
            _ => {}
        }
    }

    fn teams_enter(&mut self) {
        match self.teams.mode {
            TeamsMode::Chats => {
                if let Some(c) = self.teams.chats.get(self.teams.chat_sel) {
                    let id = c.id.clone();
                    self.teams.open_chat_id = Some(id.clone());
                    self.teams.open_channel = None;
                    self.teams.messages.clear();
                    self.teams.messages_rendered.clear();
                    self.teams.messages_links.clear();
                    self.teams.msg_sel = 0;
                    self.teams.messages_next = None;
                    self.teams.loading_more = false;
                    self.teams.unseen = 0;
                    self.teams.replying_to = None;
                    self.load_chat_messages(id, ListUpdate::Replace);
                    self.teams.focus = TeamsFocus::Messages;
                }
            }
            TeamsMode::Channels => {
                if self.teams.channels.is_empty() {
                    if let Some(t) = self.teams.teams.get(self.teams.team_sel) {
                        self.load_channels(t.id.clone());
                    }
                } else if let (Some(t), Some(ch)) = (
                    self.teams.teams.get(self.teams.team_sel),
                    self.teams.channels.get(self.teams.channel_sel),
                ) {
                    let team_id = t.id.clone();
                    let ch_id = ch.id.clone();
                    self.teams.open_channel = Some((team_id.clone(), ch_id.clone()));
                    self.teams.open_chat_id = None;
                    self.teams.messages.clear();
                    self.teams.messages_rendered.clear();
                    self.teams.messages_links.clear();
                    self.teams.msg_sel = 0;
                    self.teams.messages_next = None;
                    self.teams.loading_more = false;
                    self.teams.unseen = 0;
                    self.teams.replying_to = None;
                    self.load_channel_messages(team_id, ch_id, ListUpdate::Replace);
                    self.teams.focus = TeamsFocus::Messages;
                }
            }
        }
    }

    fn teams_send(&mut self) {
        let text = self.teams.composer.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.teams.composer.clear();
        let replying_to = self.teams.replying_to.take();

        match (replying_to, self.teams.mode) {
            // Replying in a chat: quote the original in the body, which is how
            // Teams itself represents a chat reply.
            (Some(idx), TeamsMode::Chats) => {
                let (Some(chat_id), Some(original)) = (
                    self.teams.open_chat_id.clone(),
                    self.teams.messages.get(idx),
                ) else {
                    return;
                };
                let author = original.author();
                let original = original.clone();
                let s = self.session.clone();
                self.status = format!("replying to {author}…");
                self.spawn(async move {
                    chats::send_reply(&s.graph, &chat_id, &original, &text).await?;
                    Ok(AppMessage::Done("reply sent".into()))
                });
            }
            // Channels have a real replies collection, so the reply threads.
            (Some(idx), TeamsMode::Channels) => {
                let (Some((team_id, channel_id)), Some(original)) = (
                    self.teams.open_channel.clone(),
                    self.teams.messages.get(idx),
                ) else {
                    return;
                };
                let message_id = original.id.clone();
                let s = self.session.clone();
                self.status = "replying…".into();
                self.spawn(async move {
                    channels::send_reply(&s.graph, &team_id, &channel_id, &message_id, &text)
                        .await?;
                    Ok(AppMessage::Done("reply sent".into()))
                });
            }
            (None, TeamsMode::Chats) => {
                if let Some(id) = self.teams.open_chat_id.clone() {
                    self.send_chat_message(id, text);
                }
            }
            (None, TeamsMode::Channels) => {
                if let Some((t, c)) = self.teams.open_channel.clone() {
                    self.send_channel_message(t, c, text);
                }
            }
        }
    }

    /// React to the currently-selected message with `emoji`.
    fn react_selected(&mut self, emoji: &str) {
        let Some(msg) = self.teams.messages.get(self.teams.msg_sel) else {
            return;
        };
        let message_id = msg.id.clone();
        let emoji = emoji.to_string();
        let s = self.session.clone();
        if let Some(chat_id) = self.teams.open_chat_id.clone() {
            self.spawn(async move {
                chats::set_reaction(&s.graph, &chat_id, &message_id, &emoji).await?;
                Ok(AppMessage::Done("reaction added".into()))
            });
        } else if let Some((team_id, channel_id)) = self.teams.open_channel.clone() {
            self.spawn(async move {
                channels::set_reaction(&s.graph, &team_id, &channel_id, &message_id, &emoji)
                    .await?;
                Ok(AppMessage::Done("reaction added".into()))
            });
        }
    }

    // -- overlay input -----------------------------------------------------

    fn on_key_overlay(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.overlay = None;
            return;
        }
        // Take the overlay out so we can mutate self freely, then put it back.
        let overlay = self.overlay.take();
        match overlay {
            Some(Overlay::Help) | Some(Overlay::Calendar) => {
                // any key besides Esc closes
            }
            Some(Overlay::React) => {
                if let KeyCode::Char(c @ '1'..='7') = key.code {
                    let idx = (c as u8 - b'1') as usize;
                    if let Some(emoji) = REACTIONS.get(idx) {
                        self.react_selected(emoji);
                    }
                    // picked -> close (overlay already taken)
                } else {
                    self.overlay = Some(Overlay::React); // ignore other keys
                }
            }
            Some(Overlay::Attachments) => match key.code {
                KeyCode::Char(c @ '1'..='9') => {
                    self.download_attachment((c as u8 - b'1') as usize);
                }
                _ => self.overlay = Some(Overlay::Attachments),
            },
            Some(Overlay::Links) => match key.code {
                KeyCode::Char(c @ '1'..='9') => {
                    self.open_link((c as u8 - b'1') as usize);
                }
                KeyCode::Char('y') => {
                    // Copy a link instead of opening it.
                    if let Some(url) = self.focused_links().first().cloned() {
                        self.copy_to_clipboard(&url, "link");
                    }
                }
                _ => self.overlay = Some(Overlay::Links),
            },
            Some(Overlay::Presence) => match key.code {
                KeyCode::Char(c @ '1'..='6') => {
                    let idx = (c as u8 - b'1') as usize;
                    if let Some(opt) = PRESENCE_OPTIONS.get(idx) {
                        if !self.session.config.can_write_presence() {
                            self.status =
                                "setting presence needs M365_PRESENCE_WRITE=1 + Presence.ReadWrite consent".into();
                        } else {
                            self.status = format!("setting presence to {}…", opt.label);
                            self.set_presence(opt);
                        }
                    }
                }
                KeyCode::Char('c') => {
                    if !self.session.config.can_write_presence() {
                        self.status =
                            "setting presence needs M365_PRESENCE_WRITE=1 + Presence.ReadWrite consent".into();
                    } else {
                        self.status = "clearing presence…".into();
                        self.clear_presence();
                    }
                }
                _ => self.overlay = Some(Overlay::Presence), // ignore other keys
            },
            Some(Overlay::Search { mut query }) => match key.code {
                KeyCode::Enter => {
                    let q = query.clone();
                    self.run_search(q);
                }
                KeyCode::Backspace => {
                    query.pop();
                    self.overlay = Some(Overlay::Search { query });
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    self.overlay = Some(Overlay::Search { query });
                }
                _ => self.overlay = Some(Overlay::Search { query }),
            },
            Some(Overlay::Palette { mut query, mut sel }) => {
                let matches = filter_commands(&query);
                match key.code {
                    KeyCode::Enter => {
                        if let Some((id, _)) = matches.get(sel) {
                            self.run_command(id);
                        }
                    }
                    KeyCode::Up => {
                        sel = sel.saturating_sub(1);
                        self.overlay = Some(Overlay::Palette { query, sel });
                    }
                    KeyCode::Down => {
                        if sel + 1 < matches.len() {
                            sel += 1;
                        }
                        self.overlay = Some(Overlay::Palette { query, sel });
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.overlay = Some(Overlay::Palette { query, sel: 0 });
                    }
                    KeyCode::Char(c) => {
                        query.push(c);
                        self.overlay = Some(Overlay::Palette { query, sel: 0 });
                    }
                    _ => self.overlay = Some(Overlay::Palette { query, sel }),
                }
            }
            Some(Overlay::Compose(mut c)) => self.on_key_compose(key, &mut c),
            None => {}
        }
    }

    fn on_key_compose(&mut self, key: KeyEvent, c: &mut Compose) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let is_body = c.field == 2;
        // Width the body is laid out at, so Up/Down follow what's on screen.
        let width = self.text_width_hint.get();

        match key.code {
            // -- actions --
            KeyCode::Char('s') if ctrl => {
                self.submit_compose(c);
                return;
            }
            KeyCode::Tab => c.field = next_field(c.kind.fields(), c.field),
            KeyCode::BackTab => c.field = prev_field(c.kind.fields(), c.field),
            KeyCode::Enter => {
                if is_body {
                    c.active_mut().insert('\n');
                } else if c.field == 3 {
                    let msg = c.stage_attachment();
                    if !msg.is_empty() {
                        self.status = msg;
                    }
                } else {
                    c.field = next_field(c.kind.fields(), c.field);
                }
            }
            // Drop the most recently staged attachment.
            KeyCode::Char('x') if ctrl => {
                if let Some((p, _)) = c.attachments.pop() {
                    self.status = format!("removed {}", p.display());
                }
            }

            // -- deletion --
            KeyCode::Backspace => c.active_mut().backspace(),
            KeyCode::Delete => c.active_mut().delete(),
            KeyCode::Char('w') if ctrl => c.active_mut().delete_word_before(),
            KeyCode::Char('u') if ctrl => c.active_mut().delete_to_line_start(),
            KeyCode::Char('k') if ctrl => c.active_mut().delete_to_line_end(),

            // -- movement --
            KeyCode::Left if ctrl => c.active_mut().word_left(),
            KeyCode::Right if ctrl => c.active_mut().word_right(),
            KeyCode::Left => c.active_mut().left(),
            KeyCode::Right => c.active_mut().right(),
            KeyCode::Up if is_body => c.active_mut().move_row(-1, width),
            KeyCode::Down if is_body => c.active_mut().move_row(1, width),
            KeyCode::Up => c.field = prev_field(c.kind.fields(), c.field),
            KeyCode::Down => c.field = next_field(c.kind.fields(), c.field),
            KeyCode::Home if ctrl => c.active_mut().start_of_text(),
            KeyCode::End if ctrl => c.active_mut().end_of_text(),
            KeyCode::Char('a') if ctrl => c.active_mut().home(),
            KeyCode::Char('e') if ctrl => c.active_mut().end(),
            KeyCode::Home => c.active_mut().home(),
            KeyCode::End => c.active_mut().end(),
            KeyCode::PageUp => c.active_mut().move_row(-10, width),
            KeyCode::PageDown => c.active_mut().move_row(10, width),

            // -- typing (ignore other Ctrl chords so they can't insert junk) --
            KeyCode::Char(ch) if !ctrl => c.active_mut().insert(ch),
            _ => {}
        }
        // Put the (mutated) compose overlay back.
        self.overlay = Some(Overlay::Compose(std::mem::replace(c, empty_compose())));
    }

    /// Bracketed-paste text from the terminal.
    pub fn on_paste(&mut self, text: String) {
        match self.overlay.take() {
            Some(Overlay::Compose(mut c)) => {
                c.active_mut().insert_str(&text);
                self.overlay = Some(Overlay::Compose(c));
            }
            Some(Overlay::Search { mut query }) => {
                query.push_str(text.trim());
                self.overlay = Some(Overlay::Search { query });
            }
            other => {
                self.overlay = other;
                if self.overlay.is_none()
                    && self.screen == Screen::Teams
                    && self.teams.focus == TeamsFocus::Composer
                {
                    self.teams.composer.insert_str(&text);
                }
            }
        }
    }

    fn submit_compose(&mut self, c: &mut Compose) {
        use m365_core::mail::Outgoing;

        // Recipients are required for a new mail and for a forward.
        let kind = match &c.kind {
            ComposeKind::NewMail => {
                let to = parse_recipients(&c.to.text());
                if to.is_empty() {
                    self.status = "add at least one recipient".into();
                    self.overlay = Some(Overlay::Compose(std::mem::replace(c, empty_compose())));
                    return;
                }
                Outgoing::New {
                    to,
                    subject: c.subject.text(),
                }
            }
            ComposeKind::ReplyMail { id } => Outgoing::Reply { id: id.clone() },
            ComposeKind::ReplyAllMail { id } => Outgoing::ReplyAll { id: id.clone() },
            ComposeKind::ForwardMail { id } => {
                let to = parse_recipients(&c.to.text());
                if to.is_empty() {
                    self.status = "add at least one recipient to forward to".into();
                    self.overlay = Some(Overlay::Compose(std::mem::replace(c, empty_compose())));
                    return;
                }
                Outgoing::Forward { id: id.clone(), to }
            }
        };

        let paths = c.attachments.clone();
        let body = c.body.text();
        let label = match paths.len() {
            0 => "sending…".to_string(),
            1 => "sending with 1 attachment…".to_string(),
            n => format!("sending with {n} attachments…"),
        };
        self.status = label;
        self.overlay = None;

        let s = self.session.clone();
        self.spawn(async move {
            // Read the staged files here, off the UI thread.
            let mut attachments = Vec::with_capacity(paths.len());
            for (path, _) in &paths {
                let bytes =
                    std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "attachment".to_string());
                attachments.push(m365_core::mail::OutgoingAttachment { name, bytes });
            }
            let count = attachments.len();
            mail::send_message(&s.graph, kind, &body, attachments).await?;
            Ok(AppMessage::Done(match count {
                0 => "sent".to_string(),
                1 => "sent with 1 attachment".to_string(),
                n => format!("sent with {n} attachments"),
            }))
        });
    }

    fn run_search(&mut self, query: String) {
        self.overlay = None;
        if query.trim().is_empty() {
            return;
        }
        let s = self.session.clone();
        self.status = format!("searching \"{query}\"…");
        self.outlook.msg_sel = 0;
        self.spawn(async move {
            let results = mail::search(&s.graph, &query, 50).await?;
            Ok(AppMessage::Messages {
                items: results,
                next: None,
                mode: ListUpdate::Replace,
            })
        });
    }

    fn run_command(&mut self, id: &str) {
        self.overlay = None;
        match id {
            "outlook" => self.screen = Screen::Outlook,
            "teams" => {
                self.screen = Screen::Teams;
                if self.teams.chats.is_empty() {
                    self.load_chats();
                }
            }
            "compose" => {
                self.overlay = Some(Overlay::Compose(empty_compose()));
            }
            "calendar" => self.load_calendar_and_show(),
            "chat-sender" => {
                if let Some(addr) = self.current_mail().and_then(|m| m.sender_address()) {
                    self.status = format!("opening chat with {addr}…");
                    self.open_chat_with_email(addr);
                } else {
                    self.status = "select an email first".into();
                }
            }
            "refresh" => self.refresh_current(),
            "help" => self.overlay = Some(Overlay::Help),
            "quit" => self.should_quit = true,
            _ => {}
        }
    }
}

/// Fold a freshly-fetched newest page into an existing newest-first list:
/// the fresh page wins, older already-loaded entries are kept, duplicates go.
fn merge_newest_first<T, F, K>(fresh: Vec<T>, existing: Vec<T>, key: F) -> Vec<T>
where
    F: Fn(&T) -> K,
    K: std::hash::Hash + Eq,
{
    let seen: std::collections::HashSet<K> = fresh.iter().map(&key).collect();
    fresh
        .into_iter()
        .chain(existing.into_iter().filter(|e| !seen.contains(&key(e))))
        .collect()
}

/// Flatten rendered lines to plain text for the clipboard.
fn lines_to_plain(lines: &[ratatui::text::Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shorten a URL for the status line.
fn truncate_url(url: &str) -> String {
    if url.chars().count() <= 60 {
        return url.to_string();
    }
    let head: String = url.chars().take(57).collect();
    format!("{head}…")
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Resident set size in KiB, from `/proc/self/status` (Linux).
fn read_rss_kb() -> Option<u64> {
    parse_vmrss(&std::fs::read_to_string("/proc/self/status").ok()?)
}

fn parse_vmrss(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Chronological sort key for a Teams message: creation time, then id.
fn sort_key(m: &ChatMessage) -> (i64, u64) {
    let at = m
        .created_date_time
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0);
    (at, m.id.parse::<u64>().unwrap_or(0))
}

fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Split a comma/semicolon/space-separated recipients string into addresses.
fn parse_recipients(s: &str) -> Vec<String> {
    s.split([',', ';', ' '])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Given the allowed field ids and the current one, return the next (wrapping).
fn next_field(fields: &[usize], current: usize) -> usize {
    let pos = fields.iter().position(|&f| f == current).unwrap_or(0);
    fields[(pos + 1) % fields.len()]
}

/// The previous allowed field (wrapping).
fn prev_field(fields: &[usize], current: usize) -> usize {
    let pos = fields.iter().position(|&f| f == current).unwrap_or(0);
    fields[(pos + fields.len() - 1) % fields.len()]
}

fn empty_compose() -> Compose {
    Compose::new(ComposeKind::NewMail, 0)
}

/// Move a selection index by `delta`, clamped to `[0, len)`.
fn step(idx: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    if delta < 0 {
        idx.saturating_sub((-delta) as usize)
    } else {
        (idx + delta as usize).min(max)
    }
}

/// Palette fuzzy-ish filter (case-insensitive substring on label or id).
pub fn filter_commands(query: &str) -> Vec<(&'static str, &'static str)> {
    let q = query.to_ascii_lowercase();
    PALETTE_COMMANDS
        .iter()
        .filter(|(id, label)| {
            q.is_empty() || label.to_ascii_lowercase().contains(&q) || id.contains(&q)
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{merge_newest_first, next_field, parse_recipients, step};

    #[test]
    fn messages_sort_chronologically_regardless_of_arrival_order() {
        use super::sort_key;
        let msg = |id: &str, at: &str| -> m365_core::models::ChatMessage {
            serde_json::from_value(serde_json::json!({
                "id": id,
                "createdDateTime": at,
                "body": { "contentType": "text", "content": "x" }
            }))
            .unwrap()
        };
        // Same minute, different seconds — the case Graph returned out of order.
        let mut list = [
            msg("1785859200000", "2026-08-04T15:59:50Z"),
            msg("1785859141228", "2026-08-04T15:59:01Z"),
            msg("1785859178276", "2026-08-04T15:59:38Z"),
        ];
        list.sort_by_key(sort_key);
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["1785859141228", "1785859178276", "1785859200000"],
            "oldest first"
        );

        // A missing timestamp must not panic or reorder everything else.
        let mut with_gap = [
            msg("2", "2026-08-04T16:00:00Z"),
            serde_json::from_value(serde_json::json!({ "id": "1" })).unwrap(),
        ];
        with_gap.sort_by_key(sort_key);
        assert_eq!(with_gap[0].id, "1");
    }

    #[test]
    fn merge_keeps_older_pages_and_dedups() {
        // Newest-first: the user had scrolled back to "a"; a refresh returns the
        // newest three, one of which ("e") is new.
        let existing = vec!["d", "c", "b", "a"];
        let fresh = vec!["e", "d", "c"];
        let merged = merge_newest_first(fresh, existing, |s: &&str| s.to_string());
        assert_eq!(merged, vec!["e", "d", "c", "b", "a"]);
    }

    #[test]
    fn merge_handles_empty_sides() {
        let none: Vec<&str> = vec![];
        assert_eq!(
            merge_newest_first(vec!["a"], none.clone(), |s: &&str| s.to_string()),
            vec!["a"]
        );
        assert_eq!(
            merge_newest_first(none, vec!["a", "b"], |s: &&str| s.to_string()),
            vec!["a", "b"]
        );
    }

    #[test]
    fn merge_is_idempotent_when_nothing_changed() {
        let existing = vec!["c", "b", "a"];
        let merged = merge_newest_first(vec!["c", "b", "a"], existing, |s: &&str| s.to_string());
        assert_eq!(
            merged,
            vec!["c", "b", "a"],
            "no duplicates on an unchanged refresh"
        );
    }

    #[test]
    fn reads_resident_memory_from_proc_status() {
        use super::parse_vmrss;
        let sample = "Name:\tm365\nVmPeak:\t  123456 kB\nVmRSS:\t   24680 kB\nThreads:\t9\n";
        assert_eq!(parse_vmrss(sample), Some(24680));
        assert_eq!(parse_vmrss("no such field"), None);
    }

    #[test]
    fn selection_step_clamps() {
        assert_eq!(step(0, -1, 5), 0);
        assert_eq!(step(4, 1, 5), 4);
        assert_eq!(step(2, 1, 5), 3);
        assert_eq!(step(0, 1, 0), 0, "empty list stays at 0");
    }

    #[test]
    fn compose_field_cycles_within_allowed_fields() {
        assert_eq!(next_field(&[0, 1, 2], 0), 1);
        assert_eq!(next_field(&[0, 1, 2], 2), 0);
        assert_eq!(next_field(&[2], 2), 2, "reply has only a body field");
        assert_eq!(next_field(&[0, 2], 0), 2, "forward skips subject");
    }

    #[test]
    fn recipients_split_on_common_separators() {
        assert_eq!(
            parse_recipients("a@x.pt, b@x.pt; c@x.pt d@x.pt"),
            vec!["a@x.pt", "b@x.pt", "c@x.pt", "d@x.pt"]
        );
        assert!(parse_recipients("  ,; ").is_empty());
    }
}
