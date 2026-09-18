//! Desktop notifications for messages that are actually addressed to you.
//!
//! The rule: direct messages always notify, group conversations only when you
//! are `@mentioned`. Anything else would make a busy tenant unusable.

use std::process::{Command, Stdio};

use m365_core::models::ChatMessageMention;

/// Should this message raise a notification?
///
/// `chat_type` is the Graph value (`oneOnOne`, `group`, `meeting`); channels
/// pass `None`, which is treated as group-like.
pub fn should_notify(chat_type: Option<&str>, mentions_me: bool) -> bool {
    match chat_type {
        // A one-to-one chat is by definition addressed to you.
        Some(t) if t.eq_ignore_ascii_case("oneOnOne") => true,
        // Group chats, meeting chats and channels: only when mentioned.
        _ => mentions_me,
    }
}

/// Whether the signed-in user is `@mentioned`.
///
/// Prefers the message's `mentions` array, which is exact. Chat *previews* omit
/// it, so fall back to scanning the rendered `<at>…</at>` tags in the body.
pub fn mentions_me(
    mentions: &[ChatMessageMention],
    body_html: &str,
    my_id: Option<&str>,
    my_name: Option<&str>,
) -> bool {
    if let Some(id) = my_id {
        let tagged = mentions.iter().any(|m| {
            m.mentioned
                .as_ref()
                .and_then(|i| i.user.as_ref())
                .and_then(|u| u.id.as_deref())
                == Some(id)
        });
        if tagged {
            return true;
        }
    }
    match my_name {
        Some(name) => body_mentions_name(body_html, name),
        None => false,
    }
}

/// Scan `<at ...>Name</at>` spans for one naming the user. Teams sometimes
/// renders only a first name, so a prefix match counts.
fn body_mentions_name(body_html: &str, my_name: &str) -> bool {
    let me = my_name.trim().to_lowercase();
    if me.is_empty() {
        return false;
    }
    let lower = body_html.to_lowercase();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find("<at") {
        rest = &rest[start..];
        let Some(open_end) = rest.find('>') else { break };
        let after = &rest[open_end + 1..];
        let Some(close) = after.find("</at>") else { break };
        let text = after[..close].trim();
        if !text.is_empty() && (me == text || me.starts_with(text) || text.starts_with(&me)) {
            return true;
        }
        rest = &after[close..];
    }
    false
}

/// Raise a desktop notification, falling back to the terminal bell.
pub fn send(title: &str, body: &str) {
    let body = summarise(body);
    let spawned = Command::new("notify-send")
        .args(["--app-name=Ask Ark", "--icon=mail-message-new", title, &body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok();

    if !spawned {
        // No libnotify: ring the bell, which most terminals turn into an urgency
        // hint on the window.
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x07");
        let _ = out.flush();
    }
}

/// One-line preview, short enough for a notification bubble.
fn summarise(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 140 {
        return flat;
    }
    let head: String = flat.chars().take(137).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention_of(id: &str) -> ChatMessageMention {
        serde_json::from_value(serde_json::json!({
            "mentioned": { "user": { "id": id, "displayName": "Someone" } }
        }))
        .unwrap()
    }

    #[test]
    fn direct_messages_always_notify() {
        assert!(should_notify(Some("oneOnOne"), false));
    }

    #[test]
    fn group_chats_only_notify_on_a_mention() {
        assert!(!should_notify(Some("group"), false));
        assert!(should_notify(Some("group"), true));
        assert!(!should_notify(Some("meeting"), false));
        // Channels arrive without a chat type.
        assert!(!should_notify(None, false));
        assert!(should_notify(None, true));
    }

    #[test]
    fn detects_a_mention_by_user_id() {
        let mentions = vec![mention_of("me-123")];
        assert!(mentions_me(&mentions, "", Some("me-123"), None));
        assert!(!mentions_me(&mentions, "", Some("someone-else"), None));
    }

    #[test]
    fn falls_back_to_scanning_the_body_when_previews_omit_mentions() {
        let body = r#"<p><at id="0">Alex Rivera</at> can you check this?</p>"#;
        assert!(mentions_me(&[], body, None, Some("Alex Rivera")));
        assert!(!mentions_me(&[], body, None, Some("Ana Silva")));
        // Teams often renders just the first name.
        assert!(mentions_me(
            &[],
            r#"<p><at id="0">Alex</at> ping</p>"#,
            None,
            Some("Alex Rivera")
        ));
        // A plain mention of the name in prose is not an @mention.
        assert!(!mentions_me(
            &[],
            "<p>ask Alex Rivera about it</p>",
            None,
            Some("Alex Rivera")
        ));
    }

    #[test]
    fn summary_is_bounded_and_single_line() {
        let s = summarise("hello\n\n  world   again ");
        assert_eq!(s, "hello world again");
        let long = summarise(&"x".repeat(500));
        assert_eq!(long.chars().count(), 138);
    }
}
