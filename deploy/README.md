# ask-ark — real-time stack

This folder is everything needed to give **instant push** to Ask Ark. It ships
inside the Linux release tarball, so if you have the app you already have this.

It's entirely optional. The app works fully without it, refreshing every 20
seconds; this gets you ~1 second.

## Why it exists

Microsoft Graph delivers change notifications by **POSTing to a public HTTPS
URL**. A terminal app on your laptop doesn't have one — it's behind NAT, has no
DNS name and no TLS certificate. A Cloudflare tunnel provides that URL by holding
an outbound connection open, so **no inbound ports are opened** on your machine.

```
Graph ──HTTPS──▶ cloudflared ──▶ webhook ──▶ Redis ──▶ the TUI ──▶ delta fetch
```

The notification only says *"something changed"* — the TUI then fetches the
change itself with your token. The webhook holds no credentials and never calls
Graph.

## Requirements

- **Docker** with **Compose v2** (`docker compose version`)
- **Linux**, x86_64 or aarch64 — one bundle per architecture, since it carries a
  prebuilt binary. `up.sh` checks and tells you if you took the wrong one.
- For a *permanent* hostname: a **Cloudflare account with a domain**. Without
  one you still get a working throwaway URL.

## Setup

```sh
./up.sh
```

That's it. The script checks Docker, generates the shared secret, builds the
webhook image, starts the stack, verifies the tunnel end to end, and prints the
two lines to paste into the **TUI's** `.env`:

```dotenv
M365_TUNNEL_BASE_URL=https://something-random-here.trycloudflare.com
M365_CLIENT_STATE=1f4c…
```

Then launch `ark` — the top-right corner should read **`push live`**.

Stop it with `./down.sh`. The app keeps working, just back on the 20-second
refresh.

### Named tunnel (recommended for daily use)

The default with no configuration is a **quick tunnel**: no account, no domain,
but the hostname is random and changes every restart, so you have to update
`M365_TUNNEL_BASE_URL` each time.

For a hostname that stays put:

1. At [one.dash.cloudflare.com](https://one.dash.cloudflare.com) → *Networks* →
   *Tunnels* → **Create a tunnel** → *Cloudflared*. Copy the token from the
   install command it shows you.
2. On that tunnel, add a **Published application route**: a hostname on your
   domain → service **`http://webhook:8080`**. That exact URL — `webhook` is the
   container's name on the Compose network.
3. Put the token in `.env` here:
   ```dotenv
   CLOUDFLARE_TUNNEL_TOKEN=eyJhIjoi…
   ```
4. `./up.sh` — it picks the named tunnel automatically once a token is present,
   and confirms your hostname reaches the webhook before finishing.

| | Quick tunnel | Named tunnel |
|---|---|---|
| Cloudflare account | not needed | needed |
| Domain | not needed | needed |
| Hostname | random, new on every restart | yours, permanent |
| Command | `./up.sh --quick` | `./up.sh` with a token in `.env` |

## What's in here

| File | |
|---|---|
| `up.sh` | Start it. Handles preflight, secrets, tunnel choice and verification |
| `down.sh` | Stop it and remove the containers |
| `docker-compose.yml` | The three services, with the two tunnel flavours as profiles |
| `Dockerfile` | Wraps the prebuilt `m365-webhook` in Alpine — builds in about a second |
| `.env.example` | Documented settings; `up.sh` copies it to `.env` |
| `m365-webhook` | The webhook binary, static, from the same release as `ark` |

Its `.env` is **separate from the app's** — this one configures the containers,
the app's configures the app. `up.sh` prints the two values that have to cross
over.

## Checking it works

```sh
curl https://<your-hostname>/healthz    # expect: ok
docker compose logs -f webhook          # watch notifications arrive
```

The real test: send yourself an email from your phone. It should appear in the
TUI in a second or two, well before the `⟳ synced` clock ticks over.

## When it doesn't

**`push FAILED` in the TUI.** Almost always `M365_TUNNEL_BASE_URL` not exactly
matching your tunnel hostname — Graph then reports `Failed to resolve domain`.
The URL needs the scheme, no trailing slash and no path. The status bar carries
Graph's own message.

**Nothing in the webhook logs.** Graph isn't reaching the tunnel. Check the route
points at `http://webhook:8080` and that `docker compose ps` shows the tunnel
container up.

**Notifications arrive but the TUI ignores them.** `M365_CLIENT_STATE` differs
between this `.env` and the TUI's, so they're rejected as unauthenticated. The
webhook logs `dropping notification with mismatched clientState`.

**`address already in use`.** Something else holds 6379 or 8080. Set
`REDIS_PORT` / `WEBHOOK_PORT` in `.env`; if you change the Redis port, set a
matching `M365_REDIS_URL` in the TUI's `.env` too.

**Nothing starts at all.** `docker info` is the quickest check — usually the
daemon isn't running, or your user isn't in the `docker` group.

## Notes

`M365_CLIENT_STATE` is a shared secret Graph echoes in every notification, and
the webhook verifies it. It's what stops anyone who guesses your tunnel URL from
injecting fake events, so keep the two `.env` files in sync.

**This directory's `.env` wins over your shell.** Compose normally prefers an
exported variable over the `.env` file, which means a `CLOUDFLARE_TUNNEL_TOKEN`
left in your environment would silently override the one here — and `up.sh` would
report settings that aren't the ones in use. Pointing a second connector at the
wrong tunnel is not harmless: Cloudflare balances across connectors, so
notifications get split between two webhooks with no visible symptom. `up.sh`
therefore passes the `.env` values explicitly and warns if your shell disagrees.
Keeping the token in `.env` (mode `0600`) rather than exported also keeps it out
of reach of every other process you run.

Both ports are published on `127.0.0.1` only — the tunnel reaches the webhook
over the Compose network, not through the host.

Nothing is persisted: Redis runs with saving disabled. It's a message bus, not a
database.
