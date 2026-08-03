# mo (馍)

A minimal, hackable coding-agent harness in Rust — an experimental project
inspired by [Pi](https://pi.dev/). "mo" (馍) is a Chinese bread, a playful
nod to "pie". Like Pi, the philosophy is a *minimal harness you adapt to
your workflows*: keep the core small and explicit (gateway, worker, journal,
SQLite) rather than building a sealed product.

## Architecture

```
Frontend  <->  Gateway Service  <->  Agent worker(s)
                    |                    |
                    ----> SQLite DB <-----
                          Filesystem
```

| Piece | Role |
| --- | --- |
| `mo_core` | Shared types, JSONL journal I/O, SQLite metadata DB (WAL) |
| `mo_gateway` | axum HTTP service: sessions CRUD, history, SSE live updates, worker spawn/kill (port 3000) |
| `mo_worker` | One process per session: runs the LLM agent loop (via `nah_chat`) with tools `read_file`, `edit_file`, `bash`, `spawn_subagent` |
| `frontend` | React 19 + Vite + TS UI (Vite dev proxy `/api → :3000`) |

Workers append chat/tool events to a per-session `journal.jsonl` and update
their own row in the shared SQLite DB; the gateway only reads files + DB and
controls workers by process spawn/kill. The frontend gets live updates over
SSE (`GET /api/sessions/:id/events`).

## Layout

```
Cargo.toml                 # workspace, resolver 3, edition 2024
crates/
  mo_core/                 # shared lib: types, journal, db
  mo_gateway/              # axum binary (port 3000)
  mo_worker/               # agent worker binary
frontend/                  # React 19 + Vite + TS
scripts/mock_llm.py        # OpenAI-compatible mock LLM for smoke tests
data/                      # runtime data dir (gitignored): mo.db + sessions/<id>/
```

## Run it

Prereqs: Rust 1.85+ (edition 2024), Node 20+.

```sh
# 1. Build
cargo build --workspace

# 2. Gateway (workers are spawned from target/debug/mo_worker by default;
#    the model env vars are inherited by every spawned worker)
MO_MODEL_BASE_URL=https://api.openai.com/v1 \
MO_MODEL_NAME=gpt-4o-mini \
MO_AUTH_TOKEN=sk-... \
cargo run -p mo_gateway

# 3. Frontend (dev server with /api proxy to :3000)
cd frontend && npm install && npm run dev
# -> http://localhost:5173
```

Point the UI at a workdir (an absolute path containing the files the agent
may touch — it is sandboxed there), type a prompt, and watch the session
stream live. Cancel stops the worker and its process group.

### No API key? Use the mock

```sh
python3 scripts/mock_llm.py 9001 &      # replays canned SSE responses
MO_MODEL_BASE_URL=http://127.0.0.1:9001 \
MO_MODEL_NAME=smoke-model \
cargo run -p mo_gateway
```

The mock responds based on the prompt: prompts containing `subagent` exercise
`spawn_subagent`, prompts containing `slow` start a long-running `bash sleep`
(handy for testing cancel), anything else does `read_file` + `bash`.

## Env vars

All `MO_*` vars may be set in the shell **or in a `.env` file in the project
folder** (loaded at startup; `.env` is gitignored). The gateway passes its
environment — including anything loaded from `.env` — to every spawned
worker.

| Var | Used by | Default |
| --- | --- | --- |
| `MO_DATA_DIR` | gateway + worker | `./data` |
| `MO_AGENTS_DIR` | worker | `$HOME/.agents` |
| `MO_MODEL_BASE_URL` | worker (required) | — |
| `MO_MODEL_NAME` | worker (required) | — |
| `MO_AUTH_TOKEN` | worker | unset |
| `MO_SUBAGENT_DEPTH` | worker | `0` (hard cap `3`) |
| `MO_WORKER_BIN` | gateway | sibling of `mo_gateway` exe named `mo_worker` |
| `MO_PORT` | gateway | `3000` |

## Global agent data (`$HOME/.agents`)

Global (user-level) instructions and skills live in a global agents dir —
`$HOME/.agents` by default, overridable with `MO_AGENTS_DIR`. The worker
injects them into every system prompt (root sessions **and** subagents):

```
$HOME/.agents/
  AGENTS.md                    # global instructions (optional)
  <skill-name>/SKILL.md        # global skill (optional)
  skills/<skill-name>/SKILL.md # global skill, alternate layout (optional)
```

- `AGENTS.md` is included verbatim as "Global instructions".
- Each skill's `SKILL.md` is parsed for YAML frontmatter (`name`,
  `description`); the name + description metadata and the full body are
  included in the system prompt so the model can follow them directly
  (tools are sandboxed to the session workdir, so skills must be inlined).
- Project instructions (`<workdir>/AGENTS.md`) come after the global
  instructions, so project rules can refine or override user defaults.

## API

| Endpoint | Description |
| --- | --- |
| `POST /api/sessions` `{workdir, prompt}` | create session + spawn worker |
| `GET /api/sessions` | list (newest first) |
| `GET /api/sessions/:id` | detail; liveness check flips dead workers to `failed` |
| `GET /api/sessions/:id/history?after_seq=N` | journal events after `N` |
| `GET /api/sessions/:id/events` | SSE tail: new events + synthesized status changes |
| `POST /api/sessions/:id/cancel` | SIGTERM → SIGKILL the worker process group |

Session status: `pending | running | completed | failed | cancelled`.
Journal events: `message`, `tool_call_start`, `tool_result`, `status_change`
(JSONL, `seq` + `ts` per line).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets
cargo test --workspace        # includes e2e agent-loop test vs a mock LLM
cd frontend && npm run lint && npm run build
```

## Non-goals (MVP)

No worker pool, no auth, no token-by-token streaming to the frontend, no
message compaction beyond the 1 MB tool-output cap, no static-file serving
from the gateway, no multi-user support. Extension points (more tools,
compaction, on-demand skill loading, prompt templates) are future
experiments.
