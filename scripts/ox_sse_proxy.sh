#!/bin/bash
# launchd control for the Rust ox-sse-proxy release installation.
set -uo pipefail

PLIST="$HOME/Library/LaunchAgents/__LABEL__.plist"
LABEL="__LABEL__"
PID_FILE="$HOME/.codex/ox_sse_proxy.pid"
LOG="$HOME/.codex/ox_sse_proxy.log"
PORT="${OX_PROXY_PORT:-18899}"

usage() {
  echo "usage: $0 {start|stop|restart|status|check|logs}"
}

valid_port() {
  [[ "$PORT" =~ ^[0-9]+$ ]] && (( PORT >= 1 && PORT <= 65535 ))
}

is_loaded() {
  launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1
}

listener_pids() {
  /usr/sbin/lsof -nP -t -iTCP@127.0.0.1:"$PORT" -sTCP:LISTEN 2>/dev/null || true
}

listener_command() {
  ps -ww -p "$1" -o command= 2>/dev/null | sed -e 's/^[[:space:]]*//' | head -n 1
}

check() {
  local pid command found=0
  if ! valid_port; then
    echo "check failed: invalid port '$PORT'" >&2
    return 1
  fi
  if ! is_loaded; then
    echo "check failed: launchd job $LABEL is not loaded" >&2
    return 1
  fi
  echo "check: launchd job loaded ($LABEL)"
  while IFS= read -r pid; do
    [[ -n "$pid" ]] || continue
    found=1
    command="$(listener_command "$pid")"
    if [[ "$command" == *ox-sse-proxy* && "$command" != *python* && "$command" != *Python* ]]; then
      echo "check: Rust listener pid=$pid port=127.0.0.1:$PORT command=$command"
      return 0
    fi
    echo "check failed: listener pid=$pid is not the Rust ox-sse-proxy ($command)" >&2
  done < <(listener_pids)
  if (( found == 0 )); then
    echo "check failed: no listener on 127.0.0.1:$PORT" >&2
  fi
  return 1
}

start() {
  if ! valid_port; then
    echo "start failed: invalid port '$PORT'" >&2
    return 1
  fi
  if ! is_loaded; then
    if ! launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null; then
      launchctl load "$PLIST" 2>/dev/null || {
        echo "start failed: launchctl could not load $PLIST" >&2
        return 1
      }
    fi
  fi
  local attempt
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if check >/dev/null 2>&1; then
      echo "started: launchd $LABEL, listener 127.0.0.1:$PORT"
      return 0
    fi
    sleep 0.2
  done
  echo "start failed: job loaded but Rust listener check did not pass; inspect $LOG" >&2
  return 1
}

stop() {
  if ! is_loaded; then
    echo "stopped: launchd job $LABEL was not loaded"
    return 0
  fi
  if ! launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null; then
    # A job may have exited between is_loaded and bootout. Only fall back to
    # unload while it is still loaded, otherwise the requested state is met.
    if is_loaded; then
      launchctl unload "$PLIST" 2>/dev/null || {
        is_loaded || {
          echo "stopped: launchd job $LABEL disappeared during unload"
          return 0
        }
        echo "stop failed: launchctl could not unload $LABEL" >&2
        return 1
      }
    else
      echo "stopped: launchd job $LABEL disappeared during unload"
      return 0
    fi
  fi
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if ! is_loaded; then
      echo "stopped: launchd $LABEL"
      return 0
    fi
    sleep 0.1
  done
  echo "stop failed: launchd job $LABEL remains loaded" >&2
  return 1
}

status() {
  if ! is_loaded; then
    echo "status: not loaded ($LABEL)"
    return 1
  fi
  local pid command
  pid="$(cat "$PID_FILE" 2>/dev/null || true)"
  echo "status: loaded ($LABEL), pid=${pid:-unknown}, port=127.0.0.1:$PORT"
  if [[ -n "$pid" ]]; then
    command="$(listener_command "$pid")"
    [[ -n "$command" ]] && echo "status: $command"
  fi
}

logs() {
  local lines="${OX_PROXY_LOG_LINES:-100}"
  if ! [[ "$lines" =~ ^[0-9]+$ ]]; then
    echo "logs failed: OX_PROXY_LOG_LINES must be numeric" >&2
    return 1
  fi
  [[ -f "$LOG" ]] || { echo "logs: no log file at $LOG"; return 0; }
  tail -n "$lines" "$LOG"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  restart) stop && sleep 1 && start ;;
  status) status ;;
  check) check ;;
  logs) logs ;;
  *) usage; exit 1 ;;
esac
