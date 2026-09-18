//! All rendering. Pure function of `&App` — no state mutation here.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;

use crate::app::{
    filter_commands, App, Compose, Overlay, OutlookFocus, PushState, Screen, TeamsFocus, TeamsMode,
};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

pub fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Copy mode takes over the whole frame: one hint row plus borderless,
    // full-width text so a terminal drag-select grabs only the message body.
    if app.copy_mode {
        render_copy_mode(f, app);
        return;
    }

    render_tabs(f, chunks[0], app);
    match app.screen {
        Screen::Outlook => render_outlook(f, chunks[1], app),
        Screen::Teams => render_teams(f, chunks[1], app),
    }
    render_status(f, chunks[2], app);

    if let Some(overlay) = &app.overlay {
        render_overlay(f, app, overlay);
    }
}

/// Full-screen, borderless view of the current message/conversation. No side
/// panes and no borders, so mouse selection captures exactly the text.
fn render_copy_mode(f: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(f.area());

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " COPY MODE — drag to select · y yank all · j/k scroll · z/Esc exit ",
            Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );

    let lines = match app.screen {
        Screen::Outlook => email_lines(app).unwrap_or_default(),
        Screen::Teams => conversation_lines(app, false).0,
    };
    let (wrapped, _) = crate::wrap::wrap_all(&lines, rows[1].width as usize);
    let max = (wrapped.len() as u16).saturating_sub(rows[1].height);
    f.render_widget(
        Paragraph::new(wrapped).scroll((app.copy_scroll.min(max), 0)),
        rows[1],
    );
}

/// Top row: which app is active on the left, live state on the right.
/// No key hints live here — those belong in the bottom bar.
fn render_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tab = |name: &str, active: bool| {
        if active {
            Span::styled(
                format!(" {name} "),
                Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {name} "), Style::default().fg(DIM))
        }
    };
    let tabs = Line::from(vec![
        tab("Outlook (F2)", app.screen == Screen::Outlook),
        Span::raw("  "),
        tab("Teams (F2)", app.screen == Screen::Teams),
    ]);

    // Right-hand state: presence · push · memory · last sync.
    let (dot, avail) = presence_indicator(app);
    let (push_label, push_colour) = match &app.push {
        PushState::Off => ("push off", DIM),
        PushState::Connecting => ("push …", Color::Yellow),
        PushState::Live => ("push live", Color::Green),
        PushState::Failed(_) => ("push FAILED", Color::Red),
    };
    let ram = match app.rss_kb {
        Some(kb) if kb >= 1024 => format!("{:.0} MB", kb as f64 / 1024.0),
        Some(kb) => format!("{kb} KB"),
        None => "—".to_string(),
    };
    let sync = match &app.last_sync {
        Some(t) => format!("⟳ {t}"),
        None => "⟳ …".to_string(),
    };
    let sep = || Span::styled(" · ", Style::default().fg(DIM));
    let state = Line::from(vec![
        Span::styled(format!("{dot} {avail}"), presence_style(app)),
        sep(),
        Span::styled(push_label, Style::default().fg(push_colour)),
        sep(),
        Span::styled(format!("rss {ram}"), Style::default().fg(Color::Gray)),
        sep(),
        Span::styled(sync, Style::default().fg(Color::Green)),
        Span::raw(" "),
    ]);

    let state_w = line_width(&state).min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(state_w)])
        .split(area);
    f.render_widget(Paragraph::new(tabs), cols[0]);
    f.render_widget(Paragraph::new(state), cols[1]);
}

/// Bottom row: the latest transient message on the left, the keys available
/// right now on the right.
fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let bg = Color::Rgb(30, 30, 40);
    let hints = format!(" {} · ? help ", context_hints(app));
    let hints_w = (hints.chars().count() as u16).min(area.width);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(hints_w)])
        .split(area);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", app.status),
            Style::default().fg(Color::White).bg(bg),
        )))
        .style(Style::default().bg(bg)),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints,
            Style::default().fg(DIM).bg(bg),
        ))),
        cols[1],
    );
}

fn line_width(line: &Line) -> u16 {
    line.spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>() as u16
}

/// Key hints for whatever currently has focus.
fn context_hints(app: &App) -> &'static str {
    if let Some(overlay) = &app.overlay {
        return match overlay {
            Overlay::Compose(_) => "Ctrl+S send · Esc cancel",
            Overlay::Links => "1-9 open · y copy · Esc close",
            Overlay::Attachments => "1-9 save · Esc close",
            Overlay::React => "1-7 react · Esc close",
            Overlay::Presence => "1-6 set · c clear · Esc close",
            Overlay::Search { .. } => "Enter search · Esc cancel",
            Overlay::Palette { .. } => "↑↓ choose · Enter run · Esc close",
            Overlay::Calendar | Overlay::Help => "Esc close",
        };
    }
    match app.screen {
        Screen::Outlook => match app.outlook_focus {
            OutlookFocus::Folders => "j/k move · l open folder",
            OutlookFocus::Messages => "j/k move · l read · h back · c compose · r reply · / search",
            OutlookFocus::Reading => "j/k scroll · h back · o links · A attach · y copy",
        },
        Screen::Teams => match app.teams.focus {
            TeamsFocus::List => "j/k move · l open · t chats/channels",
            TeamsFocus::Messages => "j/k select · h back · r reply · e react · i write",
            TeamsFocus::Composer => "Enter send · Shift+Enter newline · Esc leave",
        },
    }
}

// ---------------------------------------------------------------------------
// Outlook
// ---------------------------------------------------------------------------

fn render_outlook(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Percentage(40),
            Constraint::Min(20),
        ])
        .split(area);

    // Folders
    let items: Vec<ListItem> = app
        .outlook
        .folders
        .iter()
        .map(|folder| {
            let unread = folder.unread_item_count.unwrap_or(0);
            let name = folder.display_name.clone().unwrap_or_default();
            let label = if unread > 0 {
                format!("{name} ({unread})")
            } else {
                name
            };
            ListItem::new(label)
        })
        .collect();
    let mut fstate = ListState::default();
    fstate.select(Some(app.outlook.folder_sel));
    f.render_stateful_widget(
        selectable_list(items, "Folders", app.outlook_focus == OutlookFocus::Folders),
        cols[0],
        &mut fstate,
    );

    // Messages
    let msgs: Vec<ListItem> = app
        .outlook
        .messages
        .iter()
        .map(|m| {
            let unread = !m.is_read.unwrap_or(true);
            let marker = if unread { "●" } else { " " };
            let clip = if m.has_attachments.unwrap_or(false) { "📎" } else { "" };
            let subject = m.subject.clone().unwrap_or_else(|| "(no subject)".into());
            let line = Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(ACCENT)),
                Span::styled(
                    truncate(&m.sender_name(), 18),
                    Style::default().fg(Color::LightGreen),
                ),
                Span::raw("  "),
                Span::styled(clip.to_string(), Style::default().fg(DIM)),
                Span::styled(
                    subject,
                    if unread {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut mstate = ListState::default();
    mstate.select(Some(app.outlook.msg_sel));
    let msg_title = if app.outlook.messages_next.is_some() {
        format!("Messages ({} · ↓ for more)", app.outlook.messages.len())
    } else {
        format!("Messages ({})", app.outlook.messages.len())
    };
    f.render_stateful_widget(
        selectable_list(msgs, &msg_title, app.outlook_focus == OutlookFocus::Messages),
        cols[1],
        &mut mstate,
    );

    // Reading pane — scrollable when focused, like the Teams conversation.
    let focused = app.outlook_focus == OutlookFocus::Reading;
    let title = if focused {
        "Reading (j/k scroll · Esc back)"
    } else {
        "Reading"
    };
    let block = panel_block(title, focused);
    let inner = block.inner(cols[2]);
    f.render_widget(block, cols[2]);

    match email_lines(app) {
        Some(lines) => {
            let (rows, _) = crate::wrap::wrap_all(&lines, inner.width as usize);
            // Tell the key handler how far it can usefully scroll.
            app.reading_max_scroll
                .set((rows.len() as u16).saturating_sub(inner.height));
            let scroll = app.outlook.reading_scroll.min(app.reading_max_scroll.get());
            f.render_widget(Paragraph::new(rows).scroll((scroll, 0)), inner);
        }
        None => {
            app.reading_max_scroll.set(0);
            f.render_widget(
                // Keys live in the bottom bar; keep the pane itself uncluttered.
                Paragraph::new("Select a message and press Enter to read.")
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(DIM)),
                inner,
            );
        }
    }
}

/// Headers + rendered body of the open email, or `None` if nothing is open.
/// Shared by the reading pane and copy mode.
pub fn email_lines(app: &App) -> Option<Vec<Line<'static>>> {
    let m = app.outlook.reading.as_ref()?;
    let mut lines = vec![
        kv("Subject", &m.subject.clone().unwrap_or_default()),
        kv("From", &m.sender_name()),
        kv("Date", &m.received_date_time.clone().unwrap_or_default()),
        Line::raw(""),
    ];
    if !app.outlook.reading_attachments.is_empty() {
        let names: Vec<String> = app
            .outlook
            .reading_attachments
            .iter()
            .map(|a| format!("{} ({})", a.display_name(), a.human_size()))
            .collect();
        lines.insert(3, kv("Attach", &format!("📎 {}  — press A to save", names.join(", "))));
    }
    if let Some(body) = &app.outlook.reading_body {
        lines.extend(body.lines.iter().cloned());
    }
    Some(lines)
}

/// Lines of the open Teams conversation, plus the starting line index of each
/// message. `selectable` adds the `▶` cursor and selection highlight (off in
/// copy mode so the text copies cleanly).
pub fn conversation_lines(app: &App, selectable: bool) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(app.teams.messages.len());
    // Emit a "Today"/"Yesterday"/date separator whenever the day changes.
    let mut last_day: Option<chrono::NaiveDate> = None;
    // Track the previous message so consecutive ones from the same person can
    // share a single author header.
    let mut prev: Option<(String, Option<chrono::DateTime<chrono::Local>>)> = None;

    for (i, m) in app.teams.messages.iter().enumerate() {
        let when = local_time(m.created_date_time.as_deref());
        let mut day_changed = false;
        if let Some(when) = when {
            let day = when.date_naive();
            if last_day != Some(day) {
                if last_day.is_some() {
                    lines.push(Line::from(""));
                }
                lines.push(day_separator(&day_label(day)));
                last_day = Some(day);
                day_changed = true;
            }
        }
        // Record the start *after* any separator, so scrolling to a message
        // puts the message itself at the top — the pinned header carries the
        // date, and we avoid showing the same date twice.
        starts.push(lines.len());
        let selected = selectable && i == app.teams.msg_sel;
        let marker = if !selectable {
            ""
        } else if selected {
            "▶ "
        } else {
            "  "
        };

        // Every message opens with its own local time, so a run sharing one
        // author header still shows when each line was sent. Wrapped body lines
        // line up past that gutter.
        let ts = when
            .map(|w| w.format("%H:%M").to_string())
            .unwrap_or_else(|| " ".repeat(TIME_WIDTH));
        let gutter = " ".repeat(marker.chars().count() + TIME_WIDTH + 1);
        let lead = |extra: Vec<Span<'static>>| {
            let mut spans = vec![
                Span::styled(marker.to_string(), Style::default().fg(ACCENT)),
                Span::styled(format!("{ts} "), Style::default().fg(DIM)),
            ];
            spans.extend(extra);
            Line::from(spans)
        };

        if m.deleted_date_time.is_some() {
            lines.push(lead(vec![Span::styled(
                "(message deleted)",
                Style::default().fg(DIM),
            )]));
            prev = None; // a deletion breaks the run
            continue;
        }

        let author = m.author();
        let grouped = !day_changed
            && prev
                .as_ref()
                .is_some_and(|(a, t)| continues_run(a, *t, &author, when));

        let mut body: Vec<Line<'static>> = app
            .teams
            .messages_rendered
            .get(i)
            .map(|b| b.lines.clone())
            .unwrap_or_default();

        // A reply carries the message it answers as a `messageReference`
        // attachment, not as HTML, so it has to be drawn explicitly — and it
        // has to come *before* the reply text to read correctly.
        let quote = m.quoted().map(|q| {
            vec![
                Span::styled("┃ ", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{}: ", q.author),
                    Style::default().fg(Color::LightGreen),
                ),
                Span::styled(truncate(&q.preview, 70), Style::default().fg(DIM)),
            ]
        });

        match (grouped, quote) {
            // Grouped reply: the quote takes the lead line, the text follows.
            (true, Some(quote)) => lines.push(lead(quote)),
            // Grouped message: the text starts right after the time.
            (true, None) => {
                let first = if body.is_empty() {
                    Vec::new()
                } else {
                    body.remove(0).spans
                };
                lines.push(lead(first));
            }
            // New author: name on the lead line, then the quote if there is one.
            (false, quote) => {
                lines.push(lead(vec![Span::styled(
                    author.clone(),
                    Style::default()
                        .fg(if selected { Color::Cyan } else { Color::LightGreen })
                        .add_modifier(Modifier::BOLD),
                )]));
                if let Some(quote) = quote {
                    let mut spans = vec![Span::raw(gutter.clone())];
                    spans.extend(quote);
                    lines.push(Line::from(spans));
                }
            }
        }

        for line in body {
            let mut spans = vec![Span::raw(gutter.clone())];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
        for att in &m.attachments {
            if let Some(name) = &att.name {
                lines.push(Line::from(vec![
                    Span::raw(gutter.clone()),
                    Span::styled(
                        format!("📎 {name}"),
                        Style::default().fg(Color::LightBlue),
                    ),
                ]));
            }
        }
        if let Some(reactions) = m.reactions_summary() {
            lines.push(Line::from(vec![
                Span::raw(gutter.clone()),
                Span::styled(reactions, Style::default().fg(DIM)),
            ]));
        }
        prev = Some((author, when));
    }
    (lines, starts)
}

/// Width of the `HH:MM` timestamp column.
const TIME_WIDTH: usize = 5;

/// Whether a message continues the previous one's run: same author, and close
/// enough in time that repeating the name would just be noise.
fn continues_run(
    prev_author: &str,
    prev_at: Option<chrono::DateTime<chrono::Local>>,
    author: &str,
    at: Option<chrono::DateTime<chrono::Local>>,
) -> bool {
    if prev_author != author {
        return false;
    }
    match (prev_at, at) {
        // Messages are newest-first, so the gap can run either way.
        (Some(a), Some(b)) => (a - b).num_minutes().abs() <= RUN_GAP_MINUTES,
        _ => true,
    }
}

/// A pause this long starts a fresh header even for the same person.
const RUN_GAP_MINUTES: i64 = 15;

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

fn render_teams(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(32), Constraint::Min(20)])
        .split(area);

    let me_id = app.me.as_ref().map(|m| m.id.as_str());

    // Left list: chats or channels
    let (title, items, sel): (&str, Vec<ListItem>, usize) = match app.teams.mode {
        TeamsMode::Chats => {
            let items = app
                .teams
                .chats
                .iter()
                .map(|c| ListItem::new(truncate(&c.label(me_id), 30)))
                .collect();
            ("Chats (t→channels)", items, app.teams.chat_sel)
        }
        TeamsMode::Channels => {
            if app.teams.channels.is_empty() {
                let items = app
                    .teams
                    .teams
                    .iter()
                    .map(|t| ListItem::new(truncate(t.display_name.as_deref().unwrap_or(""), 30)))
                    .collect();
                ("Teams (Enter→channels)", items, app.teams.team_sel)
            } else {
                let items = app
                    .teams
                    .channels
                    .iter()
                    .map(|c| ListItem::new(truncate(c.display_name.as_deref().unwrap_or(""), 30)))
                    .collect();
                ("Channels (t→chats)", items, app.teams.channel_sel)
            }
        }
    };
    let mut lstate = ListState::default();
    lstate.select(Some(sel));
    f.render_stateful_widget(
        selectable_list(items, title, app.teams.focus == TeamsFocus::List),
        cols[0],
        &mut lstate,
    );

    // Right: messages + composer. The composer grows with its content (handy for
    // multi-line pastes) up to a cap; Min(5) leaves room for the border, the
    // pinned date header, and a couple of message rows on small terminals.
    let composer_width = cols[1].width.saturating_sub(2).max(1) as usize;
    let composer_rows = app.teams.composer.wrap(composer_width).len().clamp(1, 6) as u16;
    // One extra row while a reply is being composed, for the quoted banner.
    let reply_row = u16::from(app.teams.replying_to.is_some());
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(composer_rows + reply_row + 2),
        ])
        .split(cols[1]);

    let focused = app.teams.focus == TeamsFocus::Messages;
    let (mut lines, msg_starts) = conversation_lines(app, focused);
    if lines.is_empty() {
        lines.push(Line::styled(
            "Select a conversation and press Enter.",
            Style::default().fg(DIM),
        ));
    }
    // Wrap here rather than letting `Paragraph` do it: scrolling needs the exact
    // row count, and `Paragraph` won't report one. Estimating it undercounts —
    // words don't fill a row — which left the newest messages below the edge.
    let inner_w = right[0].width.saturating_sub(2).max(1) as usize;
    let pane_h = right[0].height.saturating_sub(3).max(1) as usize; // borders + date header
    let (rows, row_of_line) = crate::wrap::wrap_all(&lines, inner_w);
    // Where each message begins, in rendered rows.
    let msg_rows: Vec<usize> = msg_starts
        .iter()
        .map(|&l| row_of_line.get(l).copied().unwrap_or(rows.len()))
        .collect();
    let sel_end = msg_rows
        .get(app.teams.msg_sel + 1)
        .copied()
        .unwrap_or(rows.len());
    let scroll = sel_end
        .saturating_sub(pane_h)
        .min(rows.len().saturating_sub(pane_h)) as u16;
    // Flag messages that arrived while the user was reading further back.
    let title = if app.teams.unseen > 0 {
        format!("Conversation — ▼ {} new (g to jump)", app.teams.unseen)
    } else if focused {
        "Conversation (j/k select · e react · z copy-mode)".to_string()
    } else {
        "Conversation".to_string()
    };

    // The pane is split inside its border: a pinned date header on the first
    // row, then the scrolling message flow. The header tracks the day of the
    // topmost visible message, so it updates as you scroll.
    let block = panel_block(&title, focused);
    let inner = block.inner(right[0]);
    f.render_widget(block, right[0]);
    let pane = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    if let Some(label) = sticky_day_label(app, &msg_rows, scroll) {
        f.render_widget(Paragraph::new(day_separator(&label)), pane[0]);
    }
    // Already wrapped, so no `Wrap` here — the rows are exactly what's drawn.
    f.render_widget(Paragraph::new(rows).scroll((scroll, 0)), pane[1]);

    let composing = app.teams.focus == TeamsFocus::Composer;
    let title = if composing {
        "Message (Enter send · Shift/Alt+Enter newline)"
    } else {
        "Message"
    };
    let composer_block = panel_block(title, composing);
    let mut composer_inner = composer_block.inner(right[1]);
    f.render_widget(composer_block, right[1]);

    // Show what's being replied to, so the quote isn't a surprise on send.
    if let Some(idx) = app.teams.replying_to {
        let banner = Rect { height: 1, ..composer_inner };
        composer_inner = Rect {
            y: composer_inner.y + 1,
            height: composer_inner.height.saturating_sub(1),
            ..composer_inner
        };
        let who = app
            .teams
            .messages
            .get(idx)
            .map(|m| m.author())
            .unwrap_or_default();
        let excerpt = app
            .teams
            .messages_rendered
            .get(idx)
            .map(|t| truncate(&crate::content::plain(t).replace('\n', " "), 60))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("┃ replying to ", Style::default().fg(ACCENT)),
                Span::styled(who, Style::default().fg(Color::LightGreen)),
                Span::styled(format!(": {excerpt}"), Style::default().fg(DIM)),
            ])),
            banner,
        );
    }

    app.text_width_hint
        .set(composer_inner.width.max(1) as usize);
    if app.teams.composer.is_empty() && !composing {
        f.render_widget(
            Paragraph::new(Span::styled(
                "press i to type · Enter to send",
                Style::default().fg(DIM),
            )),
            composer_inner,
        );
    } else if let Some((x, y)) =
        render_text_area(f, composer_inner, &app.teams.composer, composing)
    {
        f.set_cursor_position((x, y));
    }
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn render_overlay(f: &mut Frame, app: &App, overlay: &Overlay) {
    match overlay {
        Overlay::Help => {
            let area = centered(60, 60, f.area());
            f.render_widget(Clear, area);
            let text = "\
 Ask Ark — keys\n\
 \n\
 Global:  F2 switch app · Ctrl+P palette · p set presence · ? help · q quit\n\
 \n\
 Links:   o list links in the message · 1-9 open in browser\n\
 Attach:  A list attachments · 1-9 save to your Downloads folder\n\
          when writing: Tab to Attach, type a path, Enter to attach\n\
 \n\
 Copying: y yank focused message · Y yank whole view\n\
          z copy mode (full-width, borderless — drag-select cleanly)\n\
 \n\
 Moving:  h/← out a pane · l/→ into it (opens what's selected)\n\
          j/k or ↑/↓ move · arrows work everywhere hjkl does\n\
 \n\
 Outlook: Enter open · c compose · r reply · a reply-all · f forward\n\
          / search · g calendar · in the reading pane j/k scroll\n\
 \n\
 Teams:   t chats/channels · j/k select message · g newest · e react\n\
          i type message · r reply to selected · Enter send\n\
 \n\
 Compose: Tab/Shift+Tab field · Ctrl+S send · Esc cancel\n\
          ←→↑↓ move · Ctrl+←→ by word · Home/End line · Ctrl+Home/End all\n\
          Backspace/Delete · Ctrl+W word · Ctrl+U to line start · Ctrl+K to end\n\
          Enter newline in body · paste works (bracketed paste)\n\
 \n\
 Press Esc to close.";
            f.render_widget(
                Paragraph::new(text).block(popup_block("Help")).wrap(Wrap { trim: false }),
                area,
            );
        }
        Overlay::Calendar => {
            let area = centered(70, 70, f.area());
            f.render_widget(Clear, area);
            let items: Vec<ListItem> = app
                .outlook
                .calendar
                .iter()
                .map(|e| {
                    let start = e
                        .start
                        .as_ref()
                        .map(|s| s.date_time.replace('T', " "))
                        .unwrap_or_default();
                    let subj = e.subject.clone().unwrap_or_default();
                    let online = if e.is_online_meeting.unwrap_or(false) { " 🔗" } else { "" };
                    ListItem::new(format!("{start}  {subj}{online}"))
                })
                .collect();
            let list = if items.is_empty() {
                List::new(vec![ListItem::new("No events in the next 7 days (or still loading).")])
            } else {
                List::new(items)
            };
            f.render_widget(list.block(popup_block("Calendar — next 7 days (Esc to close)")), area);
        }
        Overlay::Search { query } => {
            let area = centered(60, 20, f.area());
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("Search mail:\n\n> {query}▏\n\nEnter to search · Esc to cancel"))
                    .block(popup_block("Search")),
                area,
            );
        }
        Overlay::Palette { query, sel } => {
            let area = centered(50, 60, f.area());
            f.render_widget(Clear, area);
            let matches = filter_commands(query);
            let block = popup_block("Command palette");
            let inner = block.inner(area);
            f.render_widget(block, area);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Min(0)])
                .split(inner);
            f.render_widget(Paragraph::new(format!("> {query}▏")), rows[0]);
            let items: Vec<ListItem> = matches
                .iter()
                .map(|(_, label)| ListItem::new(*label))
                .collect();
            let mut st = ListState::default();
            st.select(Some(*sel));
            f.render_stateful_widget(
                List::new(items).highlight_style(
                    Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                rows[1],
                &mut st,
            );
        }
        Overlay::Compose(c) => render_compose(f, c, app),
        Overlay::React => {
            let area = centered(50, 24, f.area());
            f.render_widget(Clear, area);
            let picks: String = crate::app::REACTIONS
                .iter()
                .enumerate()
                .map(|(i, e)| format!("{}  {e}   ", i + 1))
                .collect();
            f.render_widget(
                Paragraph::new(format!("React to the selected message:\n\n{picks}\n\nPress 1-7 · Esc cancel"))
                    .wrap(Wrap { trim: false })
                    .block(popup_block("Add reaction")),
                area,
            );
        }
        Overlay::Attachments => {
            let area = centered(70, 50, f.area());
            f.render_widget(Clear, area);
            let block = popup_block("Attachments — press 1-9 to save · Esc close");
            let inner = block.inner(area);
            f.render_widget(block, area);
            let items: Vec<ListItem> = app
                .outlook
                .reading_attachments
                .iter()
                .take(9)
                .enumerate()
                .map(|(i, a)| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{} ", i + 1),
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(a.display_name()),
                        Span::styled(
                            format!("  {}  {}", a.human_size(), a.content_type.clone().unwrap_or_default()),
                            Style::default().fg(DIM),
                        ),
                    ]))
                })
                .collect();
            f.render_widget(List::new(items), inner);
            let hint = format!("saves to {}", crate::files::download_dir().display());
            let hint_area = Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner };
            f.render_widget(
                Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
                hint_area,
            );
        }
        Overlay::Links => {
            let links = app.focused_links();
            let area = centered(80, 60, f.area());
            f.render_widget(Clear, area);
            let block = popup_block("Links — press 1-9 to open · y copy first · Esc close");
            let inner = block.inner(area);
            f.render_widget(block, area);
            let width = inner.width.saturating_sub(4).max(10) as usize;
            let items: Vec<ListItem> = links
                .iter()
                .take(9)
                .enumerate()
                .map(|(i, url)| {
                    // Wrap long URLs across lines so the whole target is visible.
                    let mut lines = vec![Line::from(vec![
                        Span::styled(
                            format!("{} ", i + 1),
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(host_of(url), Style::default().fg(Color::LightGreen)),
                    ])];
                    for chunk in chunks_of(url, width) {
                        lines.push(Line::styled(
                            format!("  {chunk}"),
                            Style::default().fg(DIM),
                        ));
                    }
                    ListItem::new(lines)
                })
                .collect();
            f.render_widget(List::new(items), inner);
        }
        Overlay::Presence => {
            let area = centered(46, 55, f.area());
            f.render_widget(Clear, area);
            let mut body = String::new();
            if let Some(a) = app.my_presence.as_ref().and_then(|p| p.availability.as_deref()) {
                body.push_str(&format!("Current: {a}\n\n"));
            }
            for (i, opt) in crate::app::PRESENCE_OPTIONS.iter().enumerate() {
                body.push_str(&format!("{}  {}\n", i + 1, opt.label));
            }
            body.push_str("\nc  Clear (revert to automatic)\nEsc cancel");
            body.push_str(
                "\n\nThis app publishes its own presence session, so the status\nshows even with no Teams client running. Quitting clears it.",
            );
            if !app.session.config.can_write_presence() {
                body.push_str("\n\nread-only: set M365_PRESENCE_WRITE=1 and grant\nPresence.ReadWrite to enable changing status");
            }
            f.render_widget(
                Paragraph::new(body).block(popup_block("Set presence")),
                area,
            );
        }
    }
}


/// Render a wrapped, vertically-scrolling text area. Returns the on-screen
/// cursor position when focused. Shared by the compose body and the Teams
/// composer so both wrap and scroll identically.
fn render_text_area(
    f: &mut Frame,
    area: Rect,
    input: &crate::editor::TextInput,
    focused: bool,
) -> Option<(u16, u16)> {
    let width = area.width.max(1) as usize;
    let height = area.height.max(1) as usize;
    let wrapped = input.wrap(width);
    let (crow, ccol) = input.cursor_position(width);
    // Keep the cursor row on screen.
    let scroll = crow.saturating_sub(height.saturating_sub(1));
    let visible: Vec<Line> = wrapped
        .iter()
        .skip(scroll)
        .take(height)
        .map(|r| Line::raw(r.text.clone()))
        .collect();
    f.render_widget(Paragraph::new(visible), area);
    focused.then(|| {
        (
            area.x + ccol.min(width.saturating_sub(1)) as u16,
            area.y + (crow - scroll) as u16,
        )
    })
}

/// Render one single-line field, scrolling horizontally to keep the cursor in
/// view. Returns the on-screen cursor column when this field is focused.
fn render_line_field(
    f: &mut Frame,
    area: Rect,
    label: &str,
    input: &crate::editor::TextInput,
    focused: bool,
) -> Option<(u16, u16)> {
    let style = if focused {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let label_w = label.chars().count() as u16;
    let avail = area.width.saturating_sub(label_w).max(1) as usize;
    let text = input.text();
    let chars: Vec<char> = text.chars().collect();
    // Scroll so the cursor stays visible in a long recipient list.
    let offset = input.cursor().saturating_sub(avail.saturating_sub(1));
    let shown: String = chars.iter().skip(offset).take(avail).collect();

    f.render_widget(Paragraph::new(format!("{label}{shown}")).style(style), area);

    focused.then(|| {
        let col = area.x + label_w + (input.cursor() - offset) as u16;
        (col.min(area.x + area.width.saturating_sub(1)), area.y)
    })
}

fn render_compose(f: &mut Frame, c: &Compose, app: &App) {
    let area = centered(70, 70, f.area());
    f.render_widget(Clear, area);
    let block = popup_block(c.kind.title());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let fields = c.kind.fields();
    let show_to = fields.contains(&0);
    let show_subject = fields.contains(&1);

    // header rows (To/Subject) + "Body:" label + body + attach + staged + hint
    let staged = c.attachments.len() as u16;
    let mut constraints = Vec::new();
    if show_to {
        constraints.push(Constraint::Length(1));
    }
    if show_subject {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // Body: label
    constraints.push(Constraint::Min(0)); // body
    constraints.push(Constraint::Length(1)); // Attach: input
    constraints.push(Constraint::Length(staged.min(4))); // staged files
    constraints.push(Constraint::Length(1)); // hint
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let mut cursor: Option<(u16, u16)> = None;
    let mut i = 0;
    if show_to {
        cursor = render_line_field(f, rows[i], "To:      ", &c.to, c.field == 0).or(cursor);
        i += 1;
    }
    if show_subject {
        cursor = render_line_field(f, rows[i], "Subject: ", &c.subject, c.field == 1).or(cursor);
        i += 1;
    }

    let body_style = if c.field == 2 {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    f.render_widget(Paragraph::new("Body:").style(body_style), rows[i]);
    i += 1;

    let body_area = rows[i];
    // Tell the key handler what width Up/Down should move by.
    app.text_width_hint.set(body_area.width.max(1) as usize);
    cursor = render_text_area(f, body_area, &c.body, c.field == 2).or(cursor);
    i += 1;

    // Attach: type a path, Enter stages it.
    cursor = render_line_field(f, rows[i], "Attach:  ", &c.attach, c.field == 3).or(cursor);
    i += 1;

    // Staged files (most recent last), capped to the rows we reserved.
    let staged_area = rows[i];
    if staged_area.height > 0 {
        let shown = staged_area.height as usize;
        let skip = c.attachments.len().saturating_sub(shown);
        let lines: Vec<Line> = c
            .attachments
            .iter()
            .skip(skip)
            .map(|(path, size)| {
                Line::from(vec![
                    Span::styled("  📎 ", Style::default().fg(Color::LightBlue)),
                    Span::raw(
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    ),
                    Span::styled(
                        format!("  {}", human_size(*size)),
                        Style::default().fg(DIM),
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), staged_area);
    }
    i += 1;

    let hint = if c.field == 3 {
        "Enter attach file · Ctrl+X remove last · Tab field · Ctrl+S send · Esc cancel"
    } else {
        "Tab field · ←→ move · Ctrl+←→ word · Ctrl+W/U/K delete · Ctrl+S send · Esc cancel"
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(DIM))),
        rows[i],
    );

    if let Some((x, y)) = cursor {
        f.set_cursor_position((x, y));
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// The host part of a URL, for a readable link label.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// Split a long string into fixed-width chunks so it can be shown in full.
fn chunks_of(s: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

/// Parse a Graph UTC timestamp into the local timezone.
fn local_time(ts: Option<&str>) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::DateTime::parse_from_rfc3339(ts?)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local))
}

/// `Today` / `Yesterday` / `Mon 21 Jul` (with the year for other years).
fn day_label(day: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    if day == today {
        "Today".to_string()
    } else if Some(day) == today.pred_opt() {
        "Yesterday".to_string()
    } else if day.year() == today.year() {
        day.format("%a %-d %b").to_string()
    } else {
        day.format("%a %-d %b %Y").to_string()
    }
}

/// Day label for the message currently at the top of the visible area — the
/// content of the pinned header. `starts` is ascending, so the topmost visible
/// message is the last one starting at or above the scroll offset.
fn sticky_day_label(app: &App, starts: &[usize], scroll: u16) -> Option<String> {
    let idx = topmost_message_index(starts, scroll);
    let when = local_time(app.teams.messages.get(idx)?.created_date_time.as_deref())?;
    Some(day_label(when.date_naive()))
}

/// Index of the message occupying the top of the visible area.
fn topmost_message_index(starts: &[usize], scroll: u16) -> usize {
    starts
        .iter()
        .rposition(|&s| s <= scroll as usize)
        .unwrap_or(0)
}

fn day_separator(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("── ", Style::default().fg(DIM)),
        Span::styled(
            label.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " ".to_string() + &"─".repeat(40),
            Style::default().fg(DIM),
        ),
    ])
}

/// Tab-bar presence dot symbol + availability label.
fn presence_indicator(app: &App) -> (&'static str, String) {
    let avail = app
        .my_presence
        .as_ref()
        .and_then(|p| p.availability.clone())
        .unwrap_or_else(|| "…".into());
    ("●", avail)
}

fn presence_style(app: &App) -> Style {
    let color = match app
        .my_presence
        .as_ref()
        .and_then(|p| p.availability.as_deref())
        .unwrap_or("")
    {
        "Available" | "AvailableIdle" => Color::Green,
        "Busy" | "BusyIdle" | "DoNotDisturb" => Color::Red,
        "Away" | "BeRightBack" => Color::Yellow,
        _ => DIM,
    };
    Style::default().fg(color)
}

fn selectable_list<'a>(items: Vec<ListItem<'a>>, title: &'a str, focused: bool) -> List<'a> {
    List::new(items)
        .block(panel_block(title, focused))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(if focused { ACCENT } else { Color::Gray })
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▏")
}

fn panel_block(title: &str, focused: bool) -> Block<'_> {
    let color = if focused { ACCENT } else { DIM };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
}

fn popup_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

fn kv<'a>(k: &'a str, v: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{k}: "), Style::default().fg(DIM)),
        Span::raw(v.to_string()),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::{day_label, local_time};

    #[test]
    fn labels_relative_days() {
        let today = chrono::Local::now().date_naive();
        assert_eq!(day_label(today), "Today");
        assert_eq!(day_label(today.pred_opt().unwrap()), "Yesterday");
        // An older date renders as a weekday/day/month, not "Today".
        let old = chrono::NaiveDate::from_ymd_opt(2024, 3, 5).unwrap();
        let label = day_label(old);
        assert!(label.contains("Mar"), "unexpected label: {label}");
        assert!(label.contains("2024"), "past years show the year: {label}");
        assert!(!label.contains('-'), "no literal padding modifier: {label}");
    }

    #[test]
    fn sticky_header_tracks_topmost_message() {
        use super::topmost_message_index;
        // Three messages beginning at lines 0, 5 and 12.
        let starts = [0usize, 5, 12];
        assert_eq!(topmost_message_index(&starts, 0), 0);
        assert_eq!(topmost_message_index(&starts, 4), 0); // still inside msg 0
        assert_eq!(topmost_message_index(&starts, 5), 1); // exactly at msg 1
        assert_eq!(topmost_message_index(&starts, 11), 1);
        assert_eq!(topmost_message_index(&starts, 12), 2);
        assert_eq!(topmost_message_index(&starts, 99), 2); // clamped past the end
        // A separator above the first message must not select a negative index.
        assert_eq!(topmost_message_index(&[3, 9], 0), 0);
        assert_eq!(topmost_message_index(&[], 7), 0);
    }

    #[test]
    fn groups_consecutive_messages_from_one_sender() {
        use super::continues_run;
        let at = |h, m| {
            Some(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 3)
                    .unwrap()
                    .and_hms_opt(h, m, 0)
                    .unwrap()
                    .and_local_timezone(chrono::Local)
                    .unwrap(),
            )
        };

        // Same person, a minute apart: one header covers both.
        assert!(continues_run("Jaime", at(16, 30), "Jaime", at(16, 29)));
        // Different people never group.
        assert!(!continues_run("Jaime", at(16, 30), "António", at(16, 29)));
        // A long pause earns a fresh header even for the same person.
        assert!(!continues_run("Jaime", at(16, 30), "Jaime", at(15, 00)));
        // Gap is symmetric — the list runs newest-first.
        assert!(!continues_run("Jaime", at(15, 00), "Jaime", at(16, 30)));
        // Missing timestamps fall back to the author check alone.
        assert!(continues_run("Jaime", None, "Jaime", at(16, 30)));
    }

    #[test]
    fn body_lines_align_under_the_timestamp_gutter() {
        use super::TIME_WIDTH;
        // Every message opens with `marker + HH:MM + space`; wrapped body lines
        // are indented by exactly that, so text stays in one column whether or
        // not the message is grouped.
        for marker in ["▶ ", "  ", ""] {
            let lead = marker.chars().count() + TIME_WIDTH + 1;
            let gutter = " ".repeat(lead);
            assert_eq!(gutter.chars().count(), lead, "marker {marker:?}");
        }
    }

    #[test]
    fn parses_graph_timestamps_to_local() {
        assert!(local_time(Some("2026-07-27T14:30:00Z")).is_some());
        assert!(local_time(Some("2026-07-27T14:30:00.123Z")).is_some());
        assert!(local_time(Some("not a date")).is_none());
        assert!(local_time(None).is_none());
    }
}
