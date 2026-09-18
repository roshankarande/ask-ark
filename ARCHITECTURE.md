# Architecture

How ask-ark is put together and why. For installing and using it, see the
[README](README.md).

## Crate layout

A Cargo workspace with one library and two binaries:

```
crates/
  m365-core/   library — auth, Graph client, models, endpoint wrappers,
               subscriptions, and the change-event bus
  m365-tui/    package `ask-ark`, binary `ark` — the ratatui application
  webhook/     binary `m365-webhook` — axum change-notification receiver
deploy/        the real-time stack, shipped as its own release asset
```

`m365-core` knows nothing about the terminal, and the TUI holds no HTTP logic.
The webhook shares the core only for its event types.

### m365-core

| Module | Responsibility |
|---|---|
| `auth.rs` | OAuth2 device-code flow, token cache, silent refresh |
| `graph.rs` | HTTP client: throttling, retries, paging, delta, uploads |
| `config.rs` | Environment-driven settings, scope selection, Graph endpoint |
| `models.rs` | Serde structs for the Graph resources actually rendered |
| `mail.rs` `calendar.rs` `chats.rs` `channels.rs` `people.rs` | Endpoint wrappers |
| `subscriptions.rs` | Change-notification lifecycle |
| `events.rs` | Redis subscriber → typed UI events |
| `util.rs` | base64, HTML escaping |

### ask-ark

| Module | Responsibility |
|---|---|
| `app.rs` | All state and the async orchestration; key handling |
| `ui.rs` | Rendering — a pure function of `&App` |
| `content.rs` | HTML → styled terminal text, link extraction |
| `editor.rs` | Text buffer with cursor, wrapping, and editing operations |
| `wrap.rs` | Exact word wrapping, so scrolling can trust the row count |
| `clipboard.rs` `opener.rs` `files.rs` `notify.rs` | System integration |
| `navigation.rs` | Cross-links between the Outlook and Teams sides |

## Authentication

There is no first-party MSAL for Rust, so the device-code flow is implemented
directly: POST the scopes to `/devicecode`, show the user the code, then poll
`/token` until they finish in a browser. `offline_access` yields a refresh token,
and the result is cached as a `0600` JSON file in the user's config directory.
Tokens refresh silently 60 seconds before expiry.

**Scopes are deliberately stable.** Changing the requested scope set invalidates
an existing consent grant, and in tenants that disallow user consent that turns
every sign-in into an admin-approval request. New optional capabilities are
therefore opt-in rather than added to the defaults:

| Scope | Flag | Without it |
|---|---|---|
| `Team.ReadBasic.All` | `M365_TEAMS_CHANNELS` | Channels are unavailable; chats work |
| `Presence.ReadWrite` | `M365_PRESENCE_WRITE` | The status picker is read-only |

Capabilities behind an opt-in scope check `Config::can_*` before calling, so the
UI explains what's missing instead of surfacing a Graph 403.

Sign-in runs once at startup, before the terminal is put into raw mode, so the
device code can be printed normally. Command-line arguments are resolved *before*
that — `--help` and `--version` must work on a machine with no client ID, no
network and no account. Parsing them after sign-in meant asking a first-time user
to authenticate before it would tell them what the flags were.

## The Graph client

`GraphClient` wraps `reqwest` and centralises the things every call needs:

- **Endpoint** — `config::graph_base()`, normally the real Graph, overridable
  with `M365_GRAPH_BASE`. Pointing it at a local mock lets the whole UI run on
  fabricated data: useful offline, and necessary for recording a demo without a
  real mailbox on screen. Combined with a stub token cache (`auth.rs` checks a
  cached token's expiry, never its scopes) the app never contacts Microsoft.
- **Throttling** — honours `Retry-After` on 429, exponential backoff on 5xx.
- **Auth retry** — one forced token refresh on a 401.
- **Paging** — `get_page` (single page), `get_page_with_next` (page + a
  `@odata.nextLink` for incremental loading), `get_collection` (follows every
  link until exhausted).
- **Delta** — persists `@odata.deltaLink` for incremental sync.
- **Uploads** — `put_upload_chunk` for attachment upload sessions. Upload URLs
  are pre-authenticated, so the bearer token is deliberately *not* sent to them.
  Chunks are passed as `Bytes` — refcounted views into the single buffer holding
  the file — so the payload is never copied per chunk. `Content-Length` is left
  to reqwest, which derives it from the body; setting it explicitly would append
  a second, conflicting header.

The distinction between `get_page` and `get_collection` matters: an inbox view
uses `get_page`, because `get_collection` would walk the entire mailbox.

## The UI loop

The terminal thread never blocks on the network. Key handlers spawn tokio tasks;
each sends an `AppMessage` back over an mpsc channel, and the main loop applies
it to the state before the next redraw:

```
key/paste ──> App::on_key ──> tokio::spawn ──> Graph
                                  │
     redraw <── App::apply <── AppMessage (mpsc)
```

`ui.rs` is a pure function of `&App`, so rendering can never mutate state. The
one exception is a `Cell<usize>` width hint the renderer writes so the editor's
`Up`/`Down` move by the rows actually on screen.

### List update semantics

Incoming pages carry a `ListUpdate` that says how they combine with what is
already displayed:

| Mode | When | Effect |
|---|---|---|
| `Replace` | folder switch, conversation opened, search | swap the list |
| `Append` | scrolled to the end | add older items after the current ones |
| `Merge` | 20s poll, push notification, post-send refresh | fold the newest page in, dedupe by id, **keep** older pages already loaded |

`Merge` exists because a plain refresh would otherwise discard everything the
user had scrolled back through. Selection is restored by message id rather than
index, so arriving messages don't move the cursor.

Consecutive messages from one person share a single author header, the way chat
clients do. A run breaks on a different sender, a day boundary, a deleted
message, or a pause over 15 minutes — long enough that repeating the name is
useful context again.

Every message opens with `marker + HH:MM`, so a run under one name still shows
when each line was sent, and body lines are indented by exactly that gutter to
keep the text in a single column. The marker staying in the margin means every
message remains individually selectable for reactions, grouped or not.

### Conversation order

Graph answers newest-first, but a conversation is read oldest-at-the-top, so the
page is reversed on arrival and the list is stored chronologically. The list is
then **sorted by creation time** (id breaking ties — Teams ids are epoch
milliseconds) rather than trusting arrival order: Graph's ordering is not
strictly chronological, and merging pages can interleave, which showed up as
messages from the same minute appearing swapped. That shapes
everything around it: an older page fetched by scrolling up is *prepended*, a
refresh appends to the end, the newest message is the last index, and the pane
scrolls so the selected message's last line rests on the bottom row — which for
the newest message puts the freshest text directly above the composer.

Scrolling needs the row count, and that is where two earlier attempts went
wrong. `Paragraph::scroll` counts the rows it *draws*, so scrolling by logical
lines under-scrolls whenever a message wraps; estimating the wrapped height as
characters ÷ width under-counts too, because words don't fill a row exactly.
Either way the newest messages stay hidden below the bottom edge.

`wrap.rs` removes the guesswork: it word-wraps the styled lines itself, so the
row count is exact by construction. The panes render the wrapped rows *without*
`Wrap`, and scroll offsets are plain indices into them. It also hangs
continuation rows under the original indent, so wrapped message text keeps its
column instead of running back to the left margin. The reading pane and copy mode
use the same function.

If the selection is already on the newest message the view follows new arrivals;
otherwise the position is held and they are counted into `unseen`, surfaced in
the pane title as `▼ n new`. Without that a new message would appear off-screen
below wherever the user was reading.

### Replies

Channels have a real replies collection (`/messages/{id}/replies`), so a reply
there threads.

Chats have no such endpoint, and the quote is **not in the HTML**. Teams models a
chat reply as a `messageReference` attachment whose `content` is a JSON *string*
holding `messageId`, `messagePreview` and `messageSender`; the body carries only
an empty `<attachment id="…">` tag. Replies are therefore both read and written
through that attachment — rendering the body alone shows the reply text with no
sign of what it answers, which is what a first attempt at this did.

The quote is drawn *before* the reply text. For a grouped message the quote takes
the lead line beside the timestamp, so the reply's own text is never swallowed by
the line that would otherwise carry it.

Sending falls back to a plain `<blockquote>` if a tenant rejects the reference
attachment, so a reply is never lost to a failed post.

## Rendering message bodies

Mail and Teams bodies arrive as HTML. `content.rs` parses it with `html5ever`
and walks the DOM straight into styled ratatui text.

An earlier version converted HTML → Markdown → text; that produced escaping
artifacts and pipe-tables, so the intermediate step was removed.

Links are **not** printed inline. A link renders as its anchor text plus a `[n]`
marker, with the targets collected into a list the user can open or copy. This
keeps Microsoft Defender "Safelinks" — which expand a short URL into ~800
characters of tracking wrapper — from swamping the message. Those are also
unwrapped back to their original target by decoding the `url` parameter.

Parsing is done once when a message is opened and the result cached; only the
cheap layout runs per frame.

## Real-time updates

Two independent mechanisms, so the app is never dependent on the tunnel:

1. **Polling** — a 20-second timer refreshes the current view. Always on.
2. **Push** — Graph change notifications, when a tunnel is configured.

### Why a tunnel is needed at all

Graph change notifications are a *webhook*: Microsoft's servers make an inbound
HTTPS request to a URL you register. A desktop client has no public address, sits
behind NAT, and has no certificate, so Graph cannot reach it. The tunnel supplies
a public HTTPS hostname and forwards to the local webhook without opening a port.

It buys latency only — roughly 1–2 seconds instead of up to 20. Every feature
works identically without it.

### Subscription resources

Chat subscriptions must name the user explicitly —
`users/{id}/chats/getAllMessages`. The `/me/` shorthand is rejected with a 403
("User may only create user-scoped chat message subscriptions for their own
messages"), because the subscription service resolves the resource later,
outside the caller's context. Mail (`me/mailFolders('inbox')/messages`) does
accept it.

### Reporting health

Because push failures are silent by nature — the app just keeps polling — the
subscription manager reports a `PushState` (`Off`/`Connecting`/`Live`/`Failed`)
to the UI, shown in the tab bar. A misconfigured hostname previously degraded to
polling with nothing but a line in the log to show for it.

The push path is **notify-then-delta**:

```
Graph --HTTPS--> cloudflared --> webhook --> Redis --> TUI --delta fetch--> Graph
  ^                                                     |
  └──────────── TUI creates/renews subscriptions ───────┘
```

The webhook retries its Redis connection on startup: `depends_on` only waits for
the container to start, not for the server to accept connections, and exiting on
the first failure left the service dead until someone looked.

The webhook holds no Graph token and never calls Graph. It only echoes the
validation token during subscription setup, verifies `clientState`, and
republishes a small "something changed" signal. The TUI — which does hold the
delegated token — reacts with a targeted delta fetch.

Subscriptions use `includeResourceData: false`, which means **no encryption
certificate is required**. Requesting resource data inside notifications would
oblige us to manage a certificate and decrypt payloads; signalling instead
avoids that entirely, and keeps the internet-facing service credential-free.

Teams chat subscriptions expire after about an hour, so the TUI renews every 45
minutes and supplies a `lifecycleNotificationUrl` for reauthorization events.

### Packaging the stack

Push is optional, but it used to be the one part of setup that demanded a source
checkout: the root `docker-compose.yml` builds the webhook image from the
workspace. `deploy/` exists so a release user never needs that.

| | |
|---|---|
| `deploy/Dockerfile` | Copies the prebuilt `m365-webhook` into Alpine. Builds in about a second — no Rust toolchain, no source tree, no registry |
| `deploy/docker-compose.yml` | The three services, with the two tunnel flavours as Compose **profiles** |
| `deploy/up.sh` | Preflight, secret generation, tunnel choice, and verification |

Shipping the binary rather than an image keeps the bundle self-contained: no
`docker pull` from a registry we'd have to publish to and keep public, and the
tarball is auditable — you can see exactly what runs.

That binary must be **statically** linked, or Alpine reports only
`exec /usr/local/bin/m365-webhook: no such file or directory` — the dynamic
loader's unhelpful way of saying the interpreter is missing. The release workflow
therefore asserts `file` doesn't say "dynamically linked" before packaging.

Two tunnel flavours, because requiring a domain to try push out is a poor trade:

| Profile | Hostname | Needs |
|---|---|---|
| `named` | permanent, yours | a Cloudflare account, a domain, a tunnel token |
| `quick` | random `*.trycloudflare.com`, new on every restart | nothing |

`up.sh` picks `named` when a token is present and `quick` otherwise, and in quick
mode scrapes the hostname out of `cloudflared`'s logs so it can print the exact
`M365_TUNNEL_BASE_URL` to copy. That mismatch — the configured URL not matching
the actual tunnel hostname — is the failure this whole script exists to prevent;
it once degraded push silently for days. So `up.sh` finishes by fetching
`$BASE_URL/healthz` *through* the tunnel, turning a misroute into an error at
setup time instead of an opaque Graph subscription rejection later.

The guard on the token is `${CLOUDFLARE_TUNNEL_TOKEN:-…}`, not the stricter
`:?`. Compose interpolates every service regardless of the active profile, so a
required-variable guard on the `named` service would break `quick` mode for
exactly the users who have no token.

## Sending attachments

Graph's one-shot `sendMail`/`reply`/`replyAll`/`forward` actions cannot carry
files, so a message with attachments takes a different route:

1. Create a draft (`POST /me/messages`, or `createReply`/`createReplyAll`/
   `createForward` for the reply kinds).
2. For replies, the draft already holds the quoted original, so the user's text
   is prepended with a `PATCH` — the one-shot actions do this implicitly.
3. Attach each file: inline `contentBytes` under 3 MB, otherwise an upload
   session with chunks that are a multiple of 320 KiB.
4. `POST /me/messages/{id}/send`.

Messages without attachments still use the simpler one-shot path.

`send_message` takes its attachments **by value** so each file's buffer can be
moved into `Bytes` once and then sliced per chunk without copying.

The whole file is held in memory while sending. That is a deliberate trade for a
single code path — the small-attachment route has to buffer anyway to base64 it
— and is fine at ordinary attachment sizes. Streaming from disk would be the
change if very large files ever mattered; it would mean `m365-core` doing
filesystem I/O, which it otherwise avoids.

## Navigation

Panes are laid out left to right — folders, messages, reading; chat list,
conversation — so movement follows that geometry, the way a file manager does:

| Key | Effect |
|---|---|
| `h` / `←` / `Esc` | Out to the pane on the left |
| `l` / `→` | Into the pane on the right, opening the selection (same as Enter) |
| `j` / `k` / `↑` / `↓` | Move *within* the focused pane |

`j`/`k` never cross a pane boundary. In a list they move the selection; in a
reading pane or conversation they scroll, because nothing sits below to move to.
The composer is reached with `i` rather than `l`, since it is below the
conversation rather than beside it.

## Chrome layout

State and guidance are kept apart, each with one home, so neither has to compete
for space with the other:

| Position | Content | Lifetime |
|---|---|---|
| Top left | Which app is active | — |
| Top right | Presence, push health, memory, last sync | Persistent |
| Bottom left | Latest action or error | Cleared after ~10s |
| Bottom right | Keys valid for the current focus | Follows focus |

Panes carry no key hints of their own: anything the user can press appears
bottom-right, where it tracks focus rather than going stale.

Transient text expires on a 2-second local tick that also samples memory. The
tick counts how long the message has been unchanged rather than timestamping
each write, which avoids threading an expiry through the ~30 places that set it.

## External dependencies

The binary is self-contained — statically linked, TLS via rustls, so no OpenSSL
and no shared libraries. Everything beyond that is optional and invoked as a
subprocess, each with a fallback so a missing tool degrades rather than fails:

| Module | Spawns | Fallback |
|---|---|---|
| `opener.rs` | `xdg-open`, `open` | Error reported in the status bar |
| `clipboard.rs` | `wl-copy`, `xclip`, `xsel` | OSC 52 escape sequence |
| `notify.rs` | `notify-send` | Terminal bell |

Only the push path needs services, and those are containers rather than host
installs: Redis, the webhook itself, and `cloudflared`. So its one real
prerequisite is Docker with Compose v2 — see
[packaging the stack](#packaging-the-stack). `up.sh` additionally uses `curl` or
`wget` to verify the tunnel, and `openssl`/`uuidgen`/`/dev/urandom` for the shared
secret, each falling back to the next; a box with none of the three fetchers just
skips verification rather than failing.

The Rust dependency set is deliberately small: `tokio` and `reqwest` (rustls)
for I/O, `ratatui`/`crossterm` for the terminal, `html5ever` for message bodies,
`redis` for the event bus, `axum` for the webhook, plus `serde`, `chrono`,
`anyhow` and `tracing`. Base64, HTML escaping and the text editor are
hand-rolled in `util.rs` and `editor.rs` rather than pulling in crates for a few
dozen lines.

## Notifications

Only things actually addressed to the user raise one: new inbox mail, direct
chats always, group chats and channels only on an `@mention`. Anything looser
would be unusable in a busy tenant.

Mail is watched by a small `peek_inbox` fetch — the newest 15 inbox messages,
run on each poll and on every mail push. It is deliberately independent of the
folder on screen, since the message list only holds whichever folder the user is
looking at, and mail should be announced while reading elsewhere. Messages
already marked read (on a phone, in Outlook) are skipped.

Detection prefers a message's `mentions` array, which is exact. Chat *previews*
— which is all the chat list carries for conversations that aren't open — omit
it, so there is a fallback that scans the rendered `<at>…</at>` spans for the
user's name. Prose that merely contains the name is not a mention.

Two guards stop it becoming noise: notified message ids are remembered so a
20-second poll can't repeat itself, and the first sync of either mail or chats
only records a baseline — otherwise the whole inbox and every conversation's
history would announce themselves at startup.

## Handling untrusted input

Two places take data from anyone who can send you a message:

- **Attachment names** (`files.rs`) — reduced to a bare file name before
  writing, so `../../.ssh/authorized_keys` lands in the download directory as
  `authorized_keys`. Control characters, including terminal escapes, are
  stripped, and existing files are never overwritten.
- **Link targets** (`opener.rs`) — only `http`, `https` and `mailto` are handed
  to the system opener.

## Known constraints

- **No OSC 8 hyperlinks.** Ratatui diffs a grid of cells and writes each cell's
  content; an escape sequence inside a cell breaks width accounting and can be
  left unterminated on a partial redraw. Hence numbered links instead.
- **`Merge` can retain a deleted message** until the conversation is reopened,
  if it was deleted outside the refreshed window. The alternative — re-fetching
  every loaded page on each poll — costs far more.
- **Presence has two halves.** `setUserPreferredPresence` records a sticky
  preference, but what colleagues see comes from an active *presence session* —
  normally the Teams client. With no session the user reads as Offline whatever
  the preference says. The app therefore also calls `setPresence` with its own
  client ID as `sessionId`, which delegated `Presence.ReadWrite` permits, so a
  status set from the terminal is actually visible. Sessions expire (5 min–4 h),
  so the app leases an hour, renews every 30 minutes, and clears the session on
  exit rather than leaving a stale status behind. A running Teams client still
  outranks the app's session, and the session vocabulary is narrower than the
  preference one (`Busy` must be `InACall`, `DoNotDisturb` must be
  `Presenting`).
- **No call media.** Joining Teams audio/video is not a terminal capability.

## Releases

Pushing a `v*.*` tag runs `.github/workflows/release.yml`. It builds `ark` for
Linux and Windows on x64 and ARM64. Linux uses musl, so those binaries have no
libc dependency and run on any distribution. Every archive has a SHA-256
checksum.

Linux assets are `ask-ark-<arch>-linux-musl.tar.gz` and contain:

```
ark                    the application
m365-webhook            used by realtime/, never run directly
realtime/               the optional push stack (deploy/, plus its own
                        copy of m365-webhook so the docker build context
                        is self-contained)
.env.example README.md ARCHITECTURE.md LICENSE
```

This was briefly two assets — the app, and the push stack separately. That put
two similarly-named tarballs on the releases page with nothing to distinguish
them, and picking the wrong one got you a download with no `ark` in it. Shipping
one asset makes the question unanswerable-in-the-wrong-way: the push stack is a
subdirectory you either enter or ignore. The duplicated webhook binary costs a
couple of megabytes and buys a Dockerfile that is a plain `COPY`.

Windows assets are `ask-ark-<arch>-windows.zip` and contain `ark.exe` plus the
documentation. `<arch>` is `x86_64` or `aarch64`. Native runners build all four
variants, and a dependent job publishes them together.

A separate `ci.yml` runs build, `clippy -D warnings` (with `--all-targets`, so
test code is linted too), the test suite, `shellcheck` over `deploy/`, and a
Compose config validation on every push and pull request.

Asset filenames are deliberately **not** versioned, so that
`/releases/latest/download/<asset>` always resolves and the README can offer a
copy-paste install. The version lives in the directory inside each archive.
