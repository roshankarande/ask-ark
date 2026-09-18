# Ask Ark

A terminal client for **Outlook and Microsoft Teams**, in one app. Switch
between the two with `F2`.

![Reading an HTML email in the terminal, then jumping from its sender straight into a Teams chat with them](demo.gif)

- **Outlook** — read mail with proper HTML rendering, compose / reply /
  reply-all / forward, send and save attachments, search, and a 7-day calendar
  with RSVP.
- **Teams** — chats and channels, emoji reactions, shared files, and your
  presence status.
- **Live** — refreshes every 20 seconds out of the box; add a tunnel for
  instant push notifications.

Built on the Microsoft Graph API in Rust. For how it works internally, see
[ARCHITECTURE.md](ARCHITECTURE.md).

---

## Requirements

### To run it

| | |
|---|---|
| **A work or school Microsoft 365 account** | Personal accounts can't use the Teams messaging APIs |
| **An Entra app registration** | See [step 1](#1-register-an-app-in-entra) — you need a client ID |
| **Linux or Windows on x64 or ARM64** | Four native release builds are published. Linux binaries are statically linked and run on distributions including NixOS |

Nothing else. The binary has no runtime library dependencies.

### Optional helpers

These are external commands the app calls when present. Each has a fallback, so
nothing breaks if one is missing.

| Feature | Command | Package | Without it |
|---|---|---|---|
| Open links (`o`) | `xdg-open` | `xdg-utils` | Links can still be copied |
| Copy (`y`, `Y`) | `wl-copy` (Wayland), or `xclip` / `xsel` (X11) | `wl-clipboard`, `xclip`, `xsel` | Falls back to the OSC 52 escape, which most modern terminals accept |
| Notifications | `notify-send` | `libnotify` (Debian/Ubuntu: `libnotify-bin`) | Falls back to the terminal bell |

Check what you have:

```sh
for c in xdg-open wl-copy xclip xsel notify-send; do
  command -v "$c" >/dev/null && echo "✓ $c" || echo "✗ $c"
done
```

You only need **one** clipboard tool — `wl-copy` on Wayland, `xclip` or `xsel` on X11.

### For instant push (optional)

Only if you want ~1-second updates instead of the 20-second refresh:

- **Docker** and **Docker Compose** — they run the webhook, Redis and the tunnel;
  you don't install `cloudflared` or Redis yourself.
- **Nothing else.** A separate release asset contains the whole stack and a
  `./up.sh` that sets it up — see [instant push](#instant-push-optional). A
  Cloudflare account with a domain gets you a permanent hostname; without one you
  get a working throwaway `trycloudflare.com` URL instead.

### To build from source

Not needed if you use the release binary or the Nix flake.

- **Rust**, recent stable (developed against 1.96). Cargo fetches the crate
  dependencies itself; `Cargo.lock` is committed.
- **or Nix**, which supplies the toolchain via `nix develop`.

No system libraries are required: TLS is handled by rustls, so there's no
OpenSSL dependency.

## 1. Register an app in Entra

You need an app registration to get a client ID. Sign in at
[entra.microsoft.com](https://entra.microsoft.com) → *Applications* →
*App registrations* → **New registration**.

1. **Name**: anything, e.g. `ask-ark`.
2. **Supported account types**: *Accounts in this organizational directory only*.
3. **Redirect URI**: leave blank.
4. Click **Register**, then copy the **Application (client) ID** and
   **Directory (tenant) ID** from the Overview page.

Then two settings on that app:

5. **Authentication** → *Advanced settings* → **Allow public client flows** →
   **Yes**. Sign-in fails without this.
6. **API permissions** → *Add a permission* → *Microsoft Graph* →
   **Delegated permissions**, and add:

   ```
   User.Read  People.Read
   Mail.ReadWrite  Mail.Send
   Calendars.ReadWrite
   Chat.ReadWrite  ChannelMessage.Send  ChannelMessage.Read.All
   Presence.Read.All
   offline_access  openid  profile
   ```

   Two capabilities are **opt-in**, because adding a scope later forces everyone
   to re-consent. Add these now if you want them, then enable the matching flag
   in `.env`:

   | Permission | Enables | Flag |
   |---|---|---|
   | `Team.ReadBasic.All` | Teams **channels** (chats work without it) | `M365_TEAMS_CHANNELS=1` |
   | `Presence.ReadWrite` | Setting your own status | `M365_PRESENCE_WRITE=1` |

7. Click **Grant admin consent**.

> **If you're not an admin**, that button is greyed out. `ChannelMessage.Read.All`
> and `Presence.Read.All` need an administrator — ask them to register the app
> and grant consent, then use the client ID they give you. You sign in as
> yourself; the app acts on your behalf.
>
> To start without an admin, use Outlook-only scopes by setting `M365_SCOPES` in
> `.env` (see [.env.example](.env.example)).

## 2. Configure

```sh
cp .env.example .env
```

Set at minimum:

```dotenv
M365_CLIENT_ID=<Application (client) ID>
M365_TENANT_ID=<Directory (tenant) ID>
```

Leave `M365_TUNNEL_BASE_URL` empty for now — the app works fully without it,
just polling instead of instant push.

## 3. Install

Download the archive for your system from the GitHub Releases page:

| System | x64 | ARM64 |
|---|---|---|
| Linux | `ask-ark-x86_64-linux-musl.tar.gz` | `ask-ark-aarch64-linux-musl.tar.gz` |
| Windows | `ask-ark-x86_64-windows.zip` | `ask-ark-aarch64-windows.zip` |

Linux installation:

```sh
tar xzf ask-ark-*-linux-musl.tar.gz
sudo install ask-ark-*/ark /usr/local/bin/
```

On Windows, extract the ZIP and put `ark.exe` somewhere on your `PATH`.

The Linux archive contains:

| | |
|---|---|
| `ark` | **the app** — this is the one you install |
| `realtime/` | optional add-on for [instant push](#instant-push-optional); ignore it unless you want that |
| `m365-webhook` | used by `realtime/`; you never run it directly |
| `.env.example` | configuration template |
| `README.md` `ARCHITECTURE.md` `LICENSE` | documentation and license |

The Windows archive contains `ark.exe`, `.env.example`, and the documentation.

Each release also publishes a `.sha256` next to the tarball if you want to
verify it.

<details>
<summary>Other ways to install</summary>

**Nix** — from a source checkout:

```sh
nix run
```

**From source** — needs a Rust toolchain:

```sh
cargo build --release            # binary at target/release/ark
nix develop -c cargo build --release    # or via the bundled dev shell
```

</details>

## 4. Run

```sh
ark --help     # usage; needs no configuration
ark whoami     # sign in and print your identity — a good first check
ark            # launch
```

The first run prints a URL and a code: open the URL, enter the code, sign in.
The token is cached at `~/.config/ask-ark/token-cache.json` and refreshed
automatically, so you only do this once.

Use a different token cache with an absolute path:

```sh
ark --json-path /absolute/path/token-cache.json
```

By default `ark` reads environment variables and an optional `.env` in the
current directory. To load a specific file instead:

```sh
ark --env-path /absolute/path/.env
```

---

## Keys

Press `?` in the app for this list at any time.

| Scope | Keys |
|---|---|
| **Global** | `F2` switch Outlook/Teams · `Ctrl+P` command palette · `p` presence · `?` help · `q` quit |
| **Moving** | `h`/`l` out of and into a pane · `j`/`k` move within it · arrows work the same · `Tab` cycles |
| **Outlook** | `Enter` open · `c` compose · `r` reply · `a` reply-all · `f` forward · `/` search · `g` calendar |
| **Reading a mail** | `j`/`k` scroll · `Home`/`End` · `h` back to the list |
| **Teams** | `t` chats↔channels (needs `M365_TEAMS_CHANNELS=1`) · `j`/`k` select message · `g` newest · `e` react · `r` reply · `i` write · `Enter` send |
| **Attachments** | `A` list · `1`–`9` save to Downloads |
| **Links** | `o` list · `1`–`9` open in browser |
| **Copying** | `y` copy message · `Y` copy everything · `z` copy mode |
| **Writing** | `←→↑↓` move · `Ctrl+←→` by word · `Home`/`End` · `Ctrl+W`/`Ctrl+U`/`Ctrl+K` delete · `Ctrl+S` send · `Esc` cancel |

`h` and `l` work like a file manager: `l` moves right into the pane beside you,
opening whatever is selected, and `h` moves back out. `j`/`k` stay inside the
focused pane — in a reading pane or conversation they scroll the text, since
there's nothing below to move to.

Scrolling to the end of a message list or conversation loads the next 50 items.

The **top-right** shows live state — your presence, push health, memory use and
the last sync time. The **bottom-right** shows the keys available right now,
changing with whatever has focus. The bottom-left carries the most recent
message, which clears itself after a few seconds.

## Things worth knowing

**Sending attachments.** In the compose window, `Tab` to the `Attach:` field,
type a file path (`~` works) and press `Enter` to stage it. `Ctrl+X` removes the
last one. Files over 3 MB upload in chunks automatically; Graph's own ceiling is
150 MB per message.

**Saving attachments.** Messages with attachments show 📎. Press `A`, then a
number, to save to your Downloads folder. Files are never overwritten.

**Links.** Long URLs are kept out of the text — a link shows as its text plus
`[1]`. Press `o` to list them and a number to open. Microsoft "Safelinks"
tracking wrappers are unwrapped back to the real destination.

**Copying text.** Press `y` to copy a message straight to the clipboard, or `z`
for copy mode — a borderless full-width view where a normal mouse drag selects
only the message text, with no side panes in the way.

**Notifications.** You're told about things actually addressed to you:

| | Notifies |
|---|---|
| New inbox mail | Always, unless already read elsewhere |
| Direct messages | Always |
| Group chats, meeting chats | Only when someone `@mentions` you |

Mail is announced even while you're reading another folder. Uses `notify-send`
if installed, otherwise the terminal bell. Turn it off with `M365_NOTIFY=0` in
`.env`.

**Replying.** Select a message and press `r`: the composer opens with a banner
showing what you're replying to, and `Enter` sends it as a quoted reply. In a
channel it threads properly; in a chat it quotes the original the same way Teams
does. Incoming replies show the quoted text marked with `┃`.

**Conversations read like a chat.** Oldest at the top, newest at the bottom just
above the composer. Consecutive messages from the same person are grouped under
one name, each with its own timestamp down the left, and a `Today`/`Yesterday`
header stays pinned at the top of the pane as you scroll.

**Open chats update themselves.** New messages appear in the conversation you're
reading — within a second with push enabled, otherwise on the 20-second refresh.
If you're on the newest message the view follows along; if you've scrolled back
to read history it stays put and the title shows `▲ 2 new`. Press `g` to jump
back to the newest.

**Your status.** `p` sets Available / Busy / DND / Be right back / Away / Appear
offline, and it works **without Teams running** — the app publishes its own
presence session, so colleagues see the status you pick. Quitting the app clears
it, and the session is renewed automatically while the app is open.

Needs one extra permission (`Presence.ReadWrite`) plus `M365_PRESENCE_WRITE=1` in
`.env`; without them the picker is read-only.

Two quirks worth knowing:

- A **signed-in Teams client outranks the app.** If Teams is running and goes
  idle, it can pull `Available` down to `Away`. With no Teams client, what you
  pick is what shows.
- The session API's vocabulary is narrower than the picker's, so **Busy** shows
  as "In a call" and **Do not disturb** as "Presenting". The colour and the
  do-not-disturb behaviour are right; the sub-label is Microsoft's, not ours.

## Instant push (optional)

Polling every 20 seconds is the default and needs nothing. For ~1-second updates,
Graph needs a public HTTPS URL to push notifications to — which a laptop behind
NAT doesn't have. A Cloudflare tunnel provides one without opening any inbound
port.

**You already have everything** — it's the `realtime/` folder in the tarball you
downloaded in [step 3](#3-install). Nothing more to fetch:

```sh
cd ask-ark-*/realtime && ./up.sh
```

It checks Docker, generates the shared secret, starts the webhook, Redis and the
tunnel, verifies the tunnel end to end, and prints the two lines to paste into
your `.env`:

```dotenv
M365_TUNNEL_BASE_URL=https://<hostname>
M365_CLIENT_STATE=<generated secret>
```

Restart the app and the top-right should read `push live`. `./down.sh` stops it;
the app carries on with the 20-second refresh.

With no configuration you get a **throwaway** `trycloudflare.com` hostname — no
account, no domain, but it changes on every restart. For a permanent one, put a
Cloudflare tunnel token in `realtime/.env` and re-run `./up.sh`; full
instructions are in `realtime/README.md`, which is also
[deploy/README.md](deploy/README.md) here.

<details>
<summary>From a source checkout instead</summary>

The repo-root [docker-compose.yml](docker-compose.yml) is the same stack built
from source, configured through the root `.env`:

```sh
docker compose up -d --build
curl https://<your-hostname>/healthz     # expect: ok
```

Both use the same project and container names, so bringing one up replaces the
other.

</details>

### Is it actually working?

**The top-right corner tells you**, alongside your presence, memory use and last
sync time:

| | Meaning |
|---|---|
| `push live` (green) | Subscriptions are live — changes arrive in seconds |
| `push …` (yellow) | Still subscribing |
| `push FAILED` (red) | Graph rejected it; the reason appears in the status bar, and it falls back to polling |
| `push off` (grey) | No tunnel configured — normal poll-only mode |

To confirm end-to-end, send yourself an email from your phone: it should appear
within a second or two, well before the `⟳ synced` clock changes.

If it says failed, the usual cause is `M365_TUNNEL_BASE_URL` not exactly matching
your tunnel hostname — Graph reports `Failed to resolve domain ...`. Check
`docker compose logs webhook` to see requests arriving.

## Troubleshooting

**Pressing `t` says channels need a permission.** Listing your teams requires
`Team.ReadBasic.All`, which isn't requested by default. Add it to the app
registration, set `M365_TEAMS_CHANNELS=1` in `.env`, delete
`~/.config/ask-ark/token-cache.json` and sign in again. Chats need none of this.

**Sign-in asks for admin approval.** The requested scopes no longer match what
was consented. Either have an admin approve the new set, or pin the old one with
`M365_SCOPES` in `.env` and sign in again.

**Nothing arrives instantly.** Look at the push indicator in the tab bar — see
[Is it actually working?](#is-it-actually-working). Subscription errors are also
recorded in `$TMPDIR/ask-ark.log`.

**Anything else.** Logs go to `$TMPDIR/ask-ark.log`; run with `RUST_LOG=debug`
for detail.

## Development

Only needed if you're changing the code — running it doesn't require any of this.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Both run in CI on every push and pull request, along with `shellcheck` over the
`deploy/` scripts, since those ship to users untouched.

Pushing a `v*.*` tag builds static musl binaries for x86_64 and aarch64 and
publishes one tarball per architecture, with [`deploy/`](deploy/) packaged inside
it as `realtime/`. That's what the install step above downloads. Design notes and
internals live in [ARCHITECTURE.md](ARCHITECTURE.md).

## Not supported

Joining Teams calls or meetings (audio/video isn't a terminal thing — you can
still list and schedule them), and bulk chat export, which needs separately
approved Graph permissions.

## License

MIT — see [LICENSE](LICENSE).

Not affiliated with or endorsed by Microsoft. "Microsoft 365", "Outlook" and
"Teams" are trademarks of Microsoft Corporation.
