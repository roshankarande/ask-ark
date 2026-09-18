#!/bin/sh
# Bring up the ask-ark real-time stack (webhook + Redis + Cloudflare tunnel).
#
#   ./up.sh            named tunnel if CLOUDFLARE_TUNNEL_TOKEN is set, else quick
#   ./up.sh --quick    force a throwaway trycloudflare.com tunnel
#   ./up.sh --named    force the named tunnel (fails if no token)
#
# Ends by printing the two lines to paste into the TUI's .env.
#
# POSIX sh on purpose: this runs on whatever the user has.

set -eu

cd "$(dirname "$0")"

BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; OFF=''
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m'); YELLOW=$(printf '\033[33m'); OFF=$(printf '\033[0m')
fi

say()  { printf '%s\n' "$*"; }
step() { printf '%s==>%s %s\n' "$BOLD" "$OFF" "$*"; }
ok()   { printf '%s ✓ %s %s\n' "$GREEN" "$OFF" "$*"; }
warn() { printf '%s ! %s %s\n' "$YELLOW" "$OFF" "$*"; }
die()  { printf '%serror:%s %s\n' "$RED" "$OFF" "$*" >&2; exit 1; }

MODE=auto
while [ $# -gt 0 ]; do
    case "$1" in
        --quick) MODE=quick ;;
        --named) MODE=named ;;
        -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

# ── 1. Preflight ────────────────────────────────────────────────────────────
step 'Checking Docker'

command -v docker >/dev/null 2>&1 \
    || die 'docker is not installed. See https://docs.docker.com/engine/install/'

if docker compose version >/dev/null 2>&1; then
    DC='docker compose'
elif command -v docker-compose >/dev/null 2>&1; then
    DC='docker-compose'
else
    die 'Docker Compose not found. Install the compose plugin: https://docs.docker.com/compose/install/'
fi

docker info >/dev/null 2>&1 \
    || die "the Docker daemon isn't reachable. Start it (\`systemctl --user start docker\`, or Docker Desktop), or add yourself to the \`docker\` group."

[ -f m365-webhook ] || die "m365-webhook is missing from $(pwd).
It ships in the realtime/ folder of the ask-ark release tarball — re-extract
that, or build it from a source checkout:
  cargo build --release --target x86_64-unknown-linux-musl -p webhook
  cp target/x86_64-unknown-linux-musl/release/m365-webhook deploy/"

chmod +x m365-webhook 2>/dev/null || true

# Releases are per-architecture. Downloading the wrong one otherwise surfaces as
# a container that dies on "exec format error" and a health check that just times
# out, which says nothing useful.
if command -v file >/dev/null 2>&1; then
    case "$(file -b m365-webhook)" in
        *x86-64*)  bin_arch=x86_64 ;;
        *aarch64*) bin_arch=aarch64 ;;
        *)         bin_arch='' ;;
    esac
    host_arch=$(uname -m)
    if [ -n "$bin_arch" ] && [ "$bin_arch" != "$host_arch" ]; then
        die "this bundle holds a $bin_arch binary but you're on $host_arch.
Download the ask-ark-$host_arch-linux-musl.tar.gz release instead."
    fi
fi

# One HTTP getter, whichever tool the box has. Empty HAVE_FETCH means the
# verification steps get skipped rather than failing the run.
HAVE_FETCH=''
if command -v curl >/dev/null 2>&1; then
    HAVE_FETCH=curl
elif command -v wget >/dev/null 2>&1; then
    HAVE_FETCH=wget
fi
fetch() {
    case "$HAVE_FETCH" in
        curl) curl -fsS --max-time 5 "$1" 2>/dev/null ;;
        wget) wget -q -O- --timeout=5 "$1" 2>/dev/null ;;
        *)    return 1 ;;
    esac
}

# True when the failure is *this machine* being unable to resolve the name, as
# opposed to the tunnel being broken. Worth telling apart: Graph resolves the
# hostname from Microsoft's network, so push can work perfectly while a split-DNS
# or VPN resolver here refuses to look it up.
dns_failed() {
    [ "$HAVE_FETCH" = curl ] || return 1
    curl -sS --max-time 5 "$1" 2>&1 >/dev/null | grep -qi 'resolve host'
}

# ── 2. Configuration ────────────────────────────────────────────────────────
if [ ! -f .env ]; then
    step 'Creating .env from .env.example'
    cp .env.example .env
fi
chmod 600 .env 2>/dev/null || true

# Read a key from .env without sourcing it — values may contain anything.
val() { grep -E "^$1=" .env 2>/dev/null | tail -n1 | cut -d= -f2- || true; }

set_val() {
    if grep -qE "^$1=" .env; then
        awk -v k="$1" -v v="$2" '$0 ~ "^" k "=" { print k "=" v; next } { print }' \
            .env > .env.tmp
        mv .env.tmp .env
    else
        printf '%s=%s\n' "$1" "$2" >> .env
    fi
    chmod 600 .env 2>/dev/null || true
}

gen_secret() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 16
    elif command -v uuidgen >/dev/null 2>&1; then
        uuidgen | tr -d '\n'
    else
        od -An -tx1 -N16 /dev/urandom | tr -d ' \n'   # present on any POSIX box
    fi
}

CLIENT_STATE=$(val M365_CLIENT_STATE)
if [ -z "$CLIENT_STATE" ]; then
    step 'Generating a clientState secret'
    CLIENT_STATE=$(gen_secret)
    set_val M365_CLIENT_STATE "$CLIENT_STATE"
fi

TOKEN=$(val CLOUDFLARE_TUNNEL_TOKEN)

# Compose gives an exported variable precedence over .env, so a token left in
# your shell would quietly win over the one here — and this script would report
# the wrong thing. Everything below passes the .env values explicitly instead.
# (Pointing a second connector at the wrong tunnel is not a hypothetical: it
# splits notification delivery between two webhooks with no visible symptom.)
# Every value this file can set is forwarded, so .env is the single source of
# truth. An exported WEBHOOK_PORT would otherwise send the containers to one port
# while the health check below probed another, and report a healthy stack as dead.
compose() {
    CLOUDFLARE_TUNNEL_TOKEN="$TOKEN" \
    M365_CLIENT_STATE="$CLIENT_STATE" \
    REDIS_PORT="$(val REDIS_PORT)" \
    WEBHOOK_PORT="$(val WEBHOOK_PORT)" \
    RUST_LOG="$(val RUST_LOG)" \
        $DC "$@"
}

if [ -n "${CLOUDFLARE_TUNNEL_TOKEN:-}" ] && [ "${CLOUDFLARE_TUNNEL_TOKEN:-}" != "$TOKEN" ]; then
    warn 'CLOUDFLARE_TUNNEL_TOKEN is exported in your shell and differs from .env.'
    warn 'Using the value in .env. (An exported secret is readable by every'
    warn 'process you run — .env here is 0600.)'
fi

if [ "$MODE" = auto ]; then
    if [ -n "$TOKEN" ]; then MODE=named; else MODE=quick; fi
elif [ "$MODE" = named ] && [ -z "$TOKEN" ]; then
    die 'CLOUDFLARE_TUNNEL_TOKEN is empty in .env — set it, or run ./up.sh --quick'
fi

if [ "$MODE" = quick ]; then
    warn 'No tunnel token: using a throwaway trycloudflare.com hostname.'
    warn 'It changes on every restart. For a permanent URL, see README.md.'
fi

# ── 3. Start ────────────────────────────────────────────────────────────────
step "Starting the stack ($MODE tunnel)"

# Drop the other tunnel flavour first, so switching modes doesn't leave two
# connectors fighting over the same webhook.
if [ "$MODE" = quick ]; then
    compose --profile named rm -sf cloudflared >/dev/null 2>&1 || true
else
    compose --profile quick rm -sf cloudflared-quick >/dev/null 2>&1 || true
fi

compose --profile "$MODE" up -d --build

# ── 4. Verify ───────────────────────────────────────────────────────────────
WEBHOOK_PORT=$(val WEBHOOK_PORT); : "${WEBHOOK_PORT:=8080}"

if [ -n "$HAVE_FETCH" ]; then
    step 'Waiting for the webhook'
    healthy=''
    i=0
    while [ "$i" -lt 30 ]; do
        if [ "$(fetch "http://127.0.0.1:$WEBHOOK_PORT/healthz" || true)" = ok ]; then
            healthy=yes
            break
        fi
        i=$((i + 1)); sleep 1
    done
    [ -n "$healthy" ] || die "the webhook never became healthy. Check: $DC logs webhook"
    ok "webhook healthy on 127.0.0.1:$WEBHOOK_PORT"
fi

# A named tunnel's hostname isn't knowable from the token, so there's nothing to
# probe end to end. Watching for the registration line still catches the likely
# mistake — a wrong or revoked token — instead of leaving it to surface later as a
# Graph subscription failure.
if [ "$MODE" = named ]; then
    step 'Waiting for the tunnel to register'
    registered=''
    i=0
    while [ "$i" -lt 25 ]; do
        if compose logs cloudflared 2>/dev/null | grep -q 'Registered tunnel connection'; then
            registered=yes
            break
        fi
        i=$((i + 1)); sleep 1
    done
    if [ -n "$registered" ]; then
        ok 'tunnel connected to Cloudflare'
    else
        warn "the tunnel hasn't registered. If CLOUDFLARE_TUNNEL_TOKEN is wrong or"
        warn "revoked, cloudflared says so here:  $DC logs cloudflared"
    fi
fi

BASE_URL=''
if [ "$MODE" = quick ]; then
    step 'Waiting for Cloudflare to hand out a hostname'
    i=0
    while [ "$i" -lt 40 ]; do
        BASE_URL=$(compose logs cloudflared-quick 2>/dev/null \
            | grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' | tail -n1 || true)
        [ -n "$BASE_URL" ] && break
        i=$((i + 1)); sleep 1
    done
    [ -n "$BASE_URL" ] || die "no hostname appeared. Check: $DC logs cloudflared-quick"
    ok "$BASE_URL"
fi

# Prove the tunnel actually reaches the webhook, so a misrouted named tunnel is
# caught here rather than surfacing later as an opaque Graph subscription error.
if [ -n "$BASE_URL" ] && [ -n "$HAVE_FETCH" ]; then
    step 'Checking the tunnel end to end'
    reached=''
    i=0
    while [ "$i" -lt 15 ]; do
        if [ "$(fetch "$BASE_URL/healthz" || true)" = ok ]; then
            reached=yes
            break
        fi
        i=$((i + 1)); sleep 2
    done
    if [ -n "$reached" ]; then
        ok "$BASE_URL/healthz → ok"
    elif dns_failed "$BASE_URL/healthz"; then
        # Not a problem with the stack: Graph resolves this name from Microsoft's
        # network, not from here. Common behind a VPN or split-horizon resolver.
        warn "this machine can't resolve $(printf '%s' "$BASE_URL" | sed 's|https://||')."
        warn 'That does not mean push is broken — Graph resolves it independently.'
        warn "Confirm by watching '$DC logs -f webhook' once the app is running."
    else
        warn "couldn't reach $BASE_URL/healthz yet — DNS can take a moment to"
        warn 'propagate. If the app reports push FAILED, check the route points at'
        warn 'http://webhook:8080.'
    fi
fi

# ── 5. What to do next ──────────────────────────────────────────────────────
say ''
say "${BOLD}Running.${OFF} Put these in the ${BOLD}TUI's${OFF} .env — not this directory's:"
say ''
if [ -n "$BASE_URL" ]; then
    say "  M365_TUNNEL_BASE_URL=$BASE_URL"
else
    say '  M365_TUNNEL_BASE_URL=https://<the hostname you routed to http://webhook:8080>'
fi
say "  M365_CLIENT_STATE=$CLIENT_STATE"
say ''
say "${DIM}Then launch ark — the top-right corner should read 'push live'.${OFF}"
say "${DIM}Logs: $DC logs -f webhook   ·   Stop: ./down.sh${OFF}"
if [ "$MODE" = quick ]; then
    say ''
    warn 'Quick tunnel: the hostname dies with the container. Re-run ./up.sh and'
    warn 'update M365_TUNNEL_BASE_URL after every restart.'
fi
