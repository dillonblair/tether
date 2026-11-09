#!/bin/bash
# Builds Tether on this Mac, starts the relay on localhost:8080, enrolls this Mac as a
# host and starts the agent. Run it from Terminal:
#   ./scripts/mac-quickstart.sh            # reachable from this Mac only
#   ./scripts/mac-quickstart.sh --public   # also reachable from your phone, through a
#                                          # temporary Cloudflare address (needs cloudflared)
set -euo pipefail
cd "$(dirname "$0")/.."
STATE="$HOME/.tether"
RELAY="http://localhost:8080"
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

echo "==> Starting the relay"
pkill -f target/release/tether-relay 2>/dev/null || true
TETHER_ADDR=127.0.0.1:8080 TETHER_DB="$STATE/relay.db" \
  nohup ./target/release/tether-relay >"$STATE/relay.log" 2>&1 &
for _ in $(seq 1 20); do
  curl -fs "$RELAY/healthz" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fs "$RELAY/healthz" >/dev/null || { echo "The relay didn't start; see $STATE/relay.log"; exit 1; }

if [ "${1:-}" = "--public" ]; then
  command -v cloudflared >/dev/null || brew install cloudflared
  pkill -f "cloudflared tunnel --url $RELAY" 2>/dev/null || true
  nohup cloudflared tunnel --url "$RELAY" >"$STATE/tunnel.log" 2>&1 &
  echo "==> Waiting for a public address"
  for _ in $(seq 1 40); do
    PUBLIC=$(grep -o 'https://[a-z0-9-]*\.trycloudflare\.com' "$STATE/tunnel.log" | head -1 || true)
    [ -n "${PUBLIC:-}" ] && break
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

echo "==> Starting the host agent (leave this window open; Ctrl-C stops it)"
echo "    macOS will ask for Screen Recording and Accessibility permission for Terminal."
echo "    After allowing them, run this script again."
exec ./target/release/tether-agent run
