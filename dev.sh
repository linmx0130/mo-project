#!/usr/bin/env bash
#
# Run the mo project dev environment: backend + frontend together.
#
#   ./dev.sh
#
# Starts:
#   - backend : cargo run (debug)        -> REST API + SSE on http://localhost:3031
#   - frontend: npm run dev (Vite)       -> http://localhost:3030 (proxies /api to :3031)
#
# Press Ctrl+C to stop both. Environment variables (PORT, DATABASE_URL, ...) are
# inherited by the child processes, so the usual overrides keep working:
#
#   PORT=4000 ./dev.sh
#
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BACKEND_PID=""
FRONTEND_PID=""

log() {
  printf '\033[1;36m[dev]\033[0m %s\n' "$*"
}

# Kill a process and all its descendants (children first, then the parent).
stop_tree() {
  local pid="$1"
  local child
  for child in $(pgrep -P "$pid" 2>/dev/null || true); do
    stop_tree "$child"
  done
  kill "$pid" 2>/dev/null || true
}

cleanup() {
  local exit_code=$?
  trap - EXIT INT TERM
  if [[ -n "$BACKEND_PID" ]]; then
    log "Stopping backend..."
    stop_tree "$BACKEND_PID"
  fi
  if [[ -n "$FRONTEND_PID" ]]; then
    log "Stopping frontend..."
    stop_tree "$FRONTEND_PID"
  fi
  wait 2>/dev/null || true
  exit "$exit_code"
}

trap cleanup EXIT INT TERM

log "Building backend"
cargo build --workspace

log "Starting backend: cargo run (debug) -> http://localhost:3031"
(
  exec cargo run --bin mo_gateway
) &
BACKEND_PID=$!

log "Starting frontend: npm run dev -> http://localhost:3030"
(
  cd "$ROOT_DIR/frontend"
  exec npm run dev
) &
FRONTEND_PID=$!

log "Dev environment ready - open http://localhost:3031"
log "Backend health: http://localhost:3031/api/health"
log "Press Ctrl+C to stop both."

set +e
wait "$BACKEND_PID"
BACKEND_STATUS=$?
wait "$FRONTEND_PID"
FRONTEND_STATUS=$?
set -e

if [[ "$BACKEND_STATUS" -ne 0 || "$FRONTEND_STATUS" -ne 0 ]]; then
  log "A dev server exited with an error (backend=$BACKEND_STATUS, frontend=$FRONTEND_STATUS)."
  exit 1
fi
