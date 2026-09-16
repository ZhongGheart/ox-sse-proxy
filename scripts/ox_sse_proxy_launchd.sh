#!/bin/bash
# launchd wrapper for the Rust release binary. It deliberately exits successfully
# when another process already owns the configured listener.
set -uo pipefail

PORT="${OX_PROXY_PORT:-18899}"
UPSTREAM_BASE="${OX_PROXY_UPSTREAM_BASE:-https://opencode.ai/zen/go/v1}"
LOG="$HOME/.codex/ox_sse_proxy.log"
BINARY="$HOME/.codex/ox-sse-proxy"
PID_FILE="$HOME/.codex/ox_sse_proxy.pid"

if ! [[ "$PORT" =~ ^[0-9]+$ ]] || (( PORT < 1 || PORT > 65535 )); then
  echo "[launchd] invalid OX_PROXY_PORT" >> "$LOG"
  exit 1
fi

if /usr/sbin/lsof -nP -iTCP@127.0.0.1:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "[launchd] port $PORT already in use, exit without starting" >> "$LOG"
  exit 0
fi

if [[ ! -x "$BINARY" ]]; then
  echo "[launchd] missing executable $BINARY" >> "$LOG"
  exit 1
fi

echo $$ > "$PID_FILE"
exec "$BINARY" "$PORT" "$UPSTREAM_BASE"
