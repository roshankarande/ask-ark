#!/bin/sh
# Stop the ask-ark real-time stack and remove its containers.
#
#   ./down.sh            stop everything
#   ./down.sh --volumes  also drop the Redis volume (there's nothing in it worth
#                        keeping — it's a message bus, not a database)
#
# The TUI keeps working after this; it just falls back to polling every 20s.

set -eu
cd "$(dirname "$0")"

if docker compose version >/dev/null 2>&1; then
    DC='docker compose'
elif command -v docker-compose >/dev/null 2>&1; then
    DC='docker-compose'
else
    printf 'error: Docker Compose not found\n' >&2; exit 1
fi

# Both profiles, so whichever tunnel flavour is running gets stopped.
$DC --profile named --profile quick down "$@"

printf 'Stopped. Push is off; the app falls back to the 20-second refresh.\n'
