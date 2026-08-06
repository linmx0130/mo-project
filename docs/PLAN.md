This file contains the prompt that generated the MVP.


# Plan: Rust Agent Harness MVP (gateway + workers + web UI)

## Background

This is an **experimental project inspired by [Pi](https://pi.dev/)** — a minimal, hackable coding-agent harness. The project name **"mo" (馍)** is a kind of Chinese bread, a playful nod to "pie". Like Pi, the philosophy is a *minimal harness you adapt to your workflows*: keep the core small and explicit (gateway, worker, journal, SQLite) rather than building a sealed product. The MVP scope below is deliberately lean for that reason; extension points (more tools, compaction, skills, prompt templates) come later as experiments.

## Goal

Build a Rust-based agent harness in this repo (`mo-project`):

- **Gateway service** (axum HTTP server) manages agent sessions, spawns/kills worker processes, serves status + history to the frontend, owns the SQLite metadata DB.
- **Agent worker** (separate process per session) runs an LLM agent loop against an OpenAI-compatible endpoint using `nah_chat` (path dep on `/Users/mengxiaolin/Workspace/nah/nah_chat`, since v0.7.0 is unpublished). Tools: `read_file`, `edit_file`, `bash`, `spawn_subagent`.
- **Frontend** (React 19 + Vite + TS, cloned from `/Users/mengxiaolin/Workspace/rite-rsjs/frontend-prototype`) lists sessions, creates sessions, shows live status + chat/tool history via SSE.
- **IPC (decided): journal files + SQLite.** Workers append chat/tool events to a per-session `journal.jsonl` and update their own row in the shared SQLite DB (WAL mode). Gateway only reads files + DB; control channel is process spawn/kill by PID.
- **Frontend updates (decided): SSE.** Gateway tails the journal file and polls the session row, streaming new events over `GET /api/sessions/:id/events`.

Architecture:

```
Frontend  <->  Gateway Service  <->  Agent worker(s)
                     |                    |
                     ----> SQLite DB <-----
                           Filesystem
```

## Repo layout

```
mo-project/
  Cargo.toml                 # workspace, resolver 3, edition 2024
  crates/
    mo_core/                 # shared lib: types, journal, db
    mo_gateway/              # axum binary (port 3000)
    mo_worker/               # agent worker binary
  frontend/                  # copied from rite-rsjs/frontend-prototype
  data/                      # runtime data dir (gitignored): mo.db + sessions/<id>/journal.jsonl
```

Per `~/.agents/AGENTS.md`: `git init`, then all work on branch `feat/agent-harness-mvp` (never main).

## Step 1 — Workspace scaffolding

- `git init -b main`, create branch `feat/agent-harness-mvp`, `.gitignore` (`/target`, `/data`, `node_modules`, `dist`).
- Root `Cargo.toml` workspace with members `crates/*`, resolver "3".
- Key deps: `nah_chat = { path = "../nah/nah_chat" }` (worker), `tokio` (full), `axum` 0.8, `tower-http` (cors/trace), `serde`/`serde_json`, `rusqlite` (bundled), `uuid`, `chrono` (or `time`), `tracing`/`tracing-subscriber`, `futures-util`, `anyhow`/`thiserror`.

## Step 2 — `mo_core`: shared types, journal, DB

Files: `crates/mo_core/src/{lib.rs,types.rs,journal.rs,db.rs}`.

- **Types** (`types.rs`):
  - `Session { id, parent_id: Option<String>, workdir, prompt, model, status: SessionStatus, pid: Option<u32>, journal_path, created_at, updated_at, heartbeat_at, error: Option<String> }`; `SessionStatus` = `pending|running|completed|failed|cancelled`.
  - `JournalEvent { seq: u64, ts: DateTime<Utc>, kind: JournalEventKind }` with `kind` ∈ `Message(ChatMessage-ish { role, content, reasoning_content?, tool_calls? })`, `ToolCallStart { id, name, arguments }`, `ToolResult { id, name, ok, output }`, `StatusChange { status, error? }`. Serde-tagged JSON for JSONL.
- **Journal** (`journal.rs`): append-only writer (open-create-append, write line, flush — pattern from `nah/src/chat.rs:342`) and a reader that parses `journal.jsonl` into `Vec<JournalEvent>`, tolerant of a trailing partial line.
- **DB** (`db.rs`): rusqlite, `PRAGMA journal_mode=WAL; busy_timeout=5000` so gateway + multiple workers share `data/mo.db`. Migration on open:
  - `sessions(id TEXT PK, parent_id TEXT NULL, workdir TEXT, prompt TEXT, model TEXT, status TEXT, pid INTEGER NULL, journal_path TEXT, created_at TEXT, updated_at TEXT, heartbeat_at TEXT NULL, error TEXT NULL)`.
  - `journal_path` in the session row is the "index in SQLite" for the on-filesystem history (per requirement 6).
  - Functions: `create_session`, `get_session`, `list_sessions`, `update_status`, `update_heartbeat`, `set_pid`.
- Unit tests: journal round-trip incl. partial-line tolerance; db CRUD with tempdir.

## Step 3 — `mo_worker`: the agent process

Files: `crates/mo_worker/src/{main.rs,config.rs,agent.rs,tools/mod.rs,tools/{fs.rs,bash.rs,subagent.rs},prompt.rs}`.

- **CLI/env config** (`config.rs`): arg `--session-id <id>`; env `MO_DATA_DIR` (default `./data`), `MO_MODEL_BASE_URL`, `MO_MODEL_NAME`, `MO_AUTH_TOKEN`, `MO_SUBAGENT_DEPTH` (default 0, hard cap 3). Loads its `Session` row from SQLite → workdir, prompt, parent.
- **Startup**: `set_pid`, `update_status(running)`, spawn a tokio task updating `heartbeat_at` every 5s (separate rusqlite connection).
- **System prompt** (`prompt.rs`): fixed harness preamble (tool-use instructions, workdir, "you may read/write only within the workdir") + contents of `<workdir>/AGENTS.md` if present (per requirement 4).
- **Agent loop** (`agent.rs`): modeled on `nah/src/chat.rs` `ChatContext::generate`/`process_tool_calls`:
  1. messages = [system, user(prompt)] (plus depth note for subagents).
  2. `ChatClient::init(base_url, auth)` → `chat_completion_stream(model, &messages, params)`; params via `ChatCompletionParamsBuilder` with `insert("tools", json!(tool_defs))`; accumulate deltas with `ChatMessage::apply_model_response_chunk`.
  3. Append assistant message to journal; if `tool_calls` → execute each, append `ToolCallStart`/`ToolResult` events + `role:"tool"` messages, loop; else append final message, `update_status(completed)`, exit 0.
  4. On LLM error: retry with backoff (3 tries, 5s/15s/30s) → then `update_status(failed, error)`, exit 1.
- **Tools** (OpenAI nested tool JSON; args validated with serde):
  - `read_file(path)` → full UTF-8 file content (cap ~1 MB with explicit truncation note in output).
  - `edit_file(path, old_string, new_string)` → exact-replacement edit (unique match required; `replace_all` flag optional); returns **full new file content** (per requirement 3).
  - `bash(command)` → run via `sh -c` in workdir, 120s timeout, returns full stdout+stderr+exit code (cap ~1 MB).
  - `spawn_subagent(prompt)` → inserts a new session row (`parent_id` = self, same workdir, depth+1 via env), spawns `mo_worker` child process (same exe path), polls child session row (2s interval) until terminal, reads final assistant message from child journal, returns it as the tool result. Refused when depth ≥ cap.
  - Path safety (`tools/fs.rs`): all paths resolved against workdir, canonicalized, rejected if they escape the workdir root.
- **Tests**: unit tests for path safety, `edit_file` matching, `bash` timeout; an e2e agent-loop test pointing `MO_MODEL_BASE_URL` at a tiny mock axum server that replays canned chat-completion SSE responses (assistant → tool_call → final), asserting journal contents and final status. (This is how the loop gets verified without a real API key.)

## Step 4 — `mo_gateway`: HTTP service

Files: `crates/mo_gateway/src/{main.rs,state.rs,routes.rs,sse.rs,process.rs,error.rs}`.

- **State**: `Arc<AppState { data_dir, db: Mutex<rusqlite::Connection>, worker_bin: PathBuf }>`; `worker_bin` from `MO_WORKER_BIN` or sibling of `current_exe` named `mo_worker`.
- **Endpoints** (axum 0.8, `/api` prefix, port 3000, `CorsLayer::permissive()`, `TraceLayer` — same skeleton as `rite-rsjs/backend-prototype/src/main.rs`):
  - `POST /api/sessions` `{workdir, prompt}` → validate workdir exists/is dir; create session row (status `pending`, `journal_path` = `data/sessions/<id>/journal.jsonl`); spawn `mo_worker --session-id <id>` (stdout/stderr → `data/sessions/<id>/worker.log`); return session.
  - `GET /api/sessions` → list (newest first).
  - `GET /api/sessions/:id` → session detail. Liveness check: if status `running` but PID dead (`kill(pid, 0)` via `libc`), mark `failed` ("worker died").
  - `GET /api/sessions/:id/history?after_seq=N` → parse journal, return events (supports cheap re-fetch).
  - `GET /api/sessions/:id/events` → **SSE** (`Sse<impl Stream>`): per-connection tail loop — every 500 ms read new journal bytes since offset, emit new events; also poll session row, emit `StatusChange` on change; close stream when status is terminal and journal is drained.
  - `POST /api/sessions/:id/cancel` → SIGTERM then SIGKILL the PID (and its process group where feasible), `update_status(cancelled)`.
  - Error type `ApiError` implementing `IntoResponse` (the template has none — add one).
- **Tests**: integration test over the router with a temp data dir (create session with a stub worker bin, list/get/history, cancel).

## Step 5 — Frontend (React + Vite + TS)

Base: copy `rite-rsjs/frontend-prototype` → `mo-project/frontend` (React 19, Vite 8, npm, Vite proxy `/api → localhost:3000` kept). No router lib in template — keep it dependency-light: hand-rolled view switch on selected session id (prefer no new dep; add `react-router` only if it stays trivial).

- `src/api.ts` — typed fetch wrappers matching gateway DTOs (hand-duplicated, as the template does).
- `src/components/SessionList.tsx` — sidebar list w/ status badges, refreshed on interval.
- `src/components/NewSessionForm.tsx` — workdir + prompt inputs → `POST /api/sessions`.
- `src/components/SessionView.tsx` — renders journal events: user/assistant messages, reasoning (collapsible), tool calls (name + args + result, collapsible); live via `EventSource('/api/sessions/:id/events')` appending events and updating the status badge; Cancel button while running.
- Keep template styling approach (plain CSS), minimal but readable dark-friendly styles.
- `npm run lint` and `npm run build` must pass.

## Step 6 — Verification & wrap-up

1. `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test` (all crates) — must be green.
2. `npm run lint && npm run build` in `frontend/`.
3. Manual smoke: run `mo_gateway` + built `mo_worker`; `MO_MODEL_BASE_URL` pointed at the mock server (or a real endpoint if an API key is available); create a session from the UI, watch live SSE updates, verify journal + SQLite rows, cancel a running session, run a subagent scenario via prompt.
4. Write a root `README.md` (background/naming, run commands, env vars, architecture diagram) and root `AGENTS.md` (build/test commands, layout, conventions).
5. Commit only on `feat/agent-harness-mvp`, with explicit confirmation before each commit.

## Explicit non-goals (MVP)

- No worker pool (one process per session), no auth, no streaming token-by-token LLM deltas to the frontend (SSE ships completed events only), no message compaction/truncation strategy beyond the 1 MB tool-output cap, no production static-file serving (Vite dev proxy only), no multi-user support.
