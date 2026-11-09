#!/bin/bash
# Builds Tether on this Mac, starts the relay on a free local port, enrolls this Mac as a
# host and starts the agent. Ctrl-C stops everything it started. Run it from Terminal:
#   ./scripts/mac-quickstart.sh            # reachable from this Mac only
#   ./scripts/mac-quickstart.sh --public   # also reachable from your phone, through a
#                                          # temporary Cloudflare address (needs cloudflared)
# TETHER_PORT picks the relay's port; otherwise the first free one from 8787 up is used
# and remembered in ~/.tether/port.
set -euo pipefail
cd "$(dirname "$0")/.."
STATE="$HOME/.tether"
AGENT_CONFIG="$HOME/Library/Application Support/tether/agent.json"
mkdir -p "$STATE"

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
if ! command -v cargo >/dev/null; then
  echo "==> Installing Rust with Homebrew"
  brew install rust
fi
for dep in x264 opus pkgconf esbuild; do
  brew list "$dep" >/dev/null 2>&1 || brew install "$dep"
done

echo "==> Building (the first build takes a few minutes)"
cargo build --release

RELAY_PID=""
TUNNEL_PID=""
cleanup() {
  [ -n "$TUNNEL_PID" ] && kill "$TUNNEL_PID" 2>/dev/null || true
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# A relay left over from an earlier run would hold the port.
pkill -f target/release/tether-relay 2>/dev/null || true
sleep 0.3
busy() { nc -z 127.0.0.1 "$1" >/dev/null 2>&1; }
PORT="${TETHER_PORT:-$(cat "$STATE/port" 2>/dev/null || true)}"
if [ -n "$PORT" ] && busy "$PORT"; then
  echo "Port $PORT is in use by another program. Stop it, or run with TETHER_PORT=<port>"
  echo "(after changing the port, remove \"$AGENT_CONFIG\" to enroll again)."
  exit 1
fi
if [ -z "$PORT" ]; then
  PORT=8787
  while busy "$PORT"; do PORT=$((PORT + 1)); done
fi
echo "$PORT" >"$STATE/port"
RELAY="http://localhost:$PORT"

echo "==> Starting the relay on $RELAY"
TETHER_ADDR="127.0.0.1:$PORT" TETHER_DB="$STATE/relay.db" \
  ./target/release/tether-relay >"$STATE/relay.log" 2>&1 &
RELAY_PID=$!
for _ in $(seq 1 20); do
  [ "$(curl -fs "$RELAY/healthz" 2>/dev/null || true)" = "ok" ] && break
  sleep 0.5
done
if ! kill -0 "$RELAY_PID" 2>/dev/null || [ "$(curl -fs "$RELAY/healthz" 2>/dev/null || true)" != "ok" ]; then
  echo "The relay didn't start; see $STATE/relay.log"
  exit 1
fi

if [ "${1:-}" = "--public" ]; then
  command -v cloudflared >/dev/null || brew install cloudflared
  : >"$STATE/tunnel.log"
  cloudflared tunnel --url "$RELAY" >"$STATE/tunnel.log" 2>&1 &
  TUNNEL_PID=$!
  echo "==> Waiting for a public address"
  PUBLIC=""
  for _ in $(seq 1 60); do
    PUBLIC=$(grep -a -o 'https://[a-z0-9-]*\.trycloudflare\.com' "$STATE/tunnel.log" | head -1 || true)
    [ -n "$PUBLIC" ] && break
    sleep 0.5
  done
  echo "    From your phone, open: ${PUBLIC:-(no address yet; see $STATE/tunnel.log)}"
fi

if [ ! -f "$AGENT_CONFIG" ]; then
  open "$RELAY"
  echo
  echo "In the page that just opened: create an account, then press \"Get code\"."
  read -r -p "Paste the code here and press Return: " CODE
  ./target/release/tether-agent enroll --relay "$RELAY" --code "$CODE"
fi

echo "==> Starting the host agent (leave this window open; Ctrl-C stops everything)"
echo "    macOS will ask for Screen Recording and Accessibility permission for Terminal."
echo "    After allowing them, run this script again."
./target/release/tether-agent run
