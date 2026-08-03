# mo-project

Experimental Rust agent harness (gateway + worker + web UI). See README.md.

## Layout

- `crates/mo_core/` — shared types, JSONL journal, TOML config, SQLite DB (WAL)
- `crates/mo_gateway/` — axum HTTP service (port 3031), SSE, process mgmt
- `crates/mo_worker/` — agent worker binary (spawned per session)
- `frontend/` — React 19 + Vite + TS UI (dev server on 3030)
- `mo.toml.example` — example config file (copy to `mo.toml`); search path
  `$PWD/mo.toml` > `$HOME/.config/mo-agents/mo.toml`, or `--config <file>`
- `scripts/mock_llm.py` — OpenAI-compatible mock LLM for smoke tests
- `data/` — runtime dir (gitignored): `mo.db` + `sessions/<id>/journal.jsonl`

## Commands

```sh
cargo build --workspace
cargo test --workspace        # all unit + integration tests
cargo clippy --all-targets    # keep at 0 warnings
cargo fmt --check             # keep formatted

cd frontend
npm install
npm run lint
npm run build
npm run dev                   # dev server on :3030, /api proxied to :3031
```

## Conventions

- Rust: edition 2024, workspace resolver 3; shared deps in `[workspace.dependencies]`.
- The gateway and every worker open the same SQLite DB (`data/mo.db`) in WAL
  mode with a busy timeout — never hold the `Mutex<Connection>` across awaits.
- Workers are the single writers of their session journal; the gateway only
  reads files + DB and controls workers via spawn/kill (process group).
- Configuration lives in `mo.toml` (models, port, data dir, agents dir,
  subagent depth); the gateway passes the resolved values to spawned workers
  via env. `MO_*` env vars are only a fallback when no config file exists.
- Tool outputs are capped at ~1 MB; paths are sandboxed to the session workdir.
- Frontend: no router lib, hand-rolled view switch; types hand-duplicated in
  `src/api.ts`; SSE `after_seq` cursor + `seq: null` synthetic status events.

## Branch policy

Per `~/.agents/AGENTS.md`, feature work goes on a branch; the initial MVP was
committed directly to `main` with explicit user approval. When in doubt,
follow the user's instruction for the current task.
