# codex-openai-proxy Plan

## Goals and scope
- Provide a drop-in OpenAI-compatible endpoint (chat/completions + responses) that forwards to Codex, so tools like Cursor can point `base_url` here while using existing Codex auth/session.
- Preserve Codex behavior (tools, approvals, sandbox) while returning OpenAI-shaped responses, including streaming.

## Current state
- Binary: `codex-openai-proxy` (Axum server) listens on `CODEX_OPENAI_PROXY_ADDR` (default `127.0.0.1:11435`).
- Endpoints: `/v1/chat/completions` and `/v1/responses` (same handler for now).
- Streaming: SSE `stream=true` supported; emits delta chunks plus final finish chunk.
- Conversation reuse: optional `conversation_id` (Codex thread id) accepted and echoed back.
- Execution: submits a `UserTurn` with `AskForApproval::Never`, `SandboxPolicy::ReadOnly`, cwd = current dir.
- Tool calls: maps Codex `FunctionCall` / `CustomToolCall` into OpenAI `tool_calls` (both stream and non-stream).
- Finish reason: `tool_calls` if any tool call was seen, otherwise `stop`.
- Model mapping: small alias table (`gpt-4.1*`, `gpt-4o*`, `o3-mini`, `o1-mini`, `o1-preview`) with identity fallback.
- Errors: OpenAI-style JSON `{ "error": { message, type } }`.

## Known gaps / limitations
- `/v1/responses` is just an alias to chat; OpenAI Responses has richer schema (input/response_format/audio/etc.) that we do not mirror yet.
- Tool call payload is minimal (id/name/args); no type-specific metadata, no call order/index beyond fixed 0.
- Streaming finish chunk is synthetic; we do not forward Codex-native finish codes (length/content_filter/etc.).
- Model mapping is shallow; no validation or vendor-specific mapping; unknown models pass through.
- No rate limiting, auth gating, or request logging; relies entirely on existing Codex auth state.
- No retry/backoff to Codex; thread errors are surfaced directly.

## Next concrete tasks
1) `/v1/responses` parity: add proper request/response structs and map Codex events accordingly.
2) Tool calls:
   - Preserve call index/order.
   - Consider emitting aggregated `tool_calls` in the final non-stream chunk for streams.
   - Add optional passthrough of arguments as structured JSON if available.
3) Finish reasons: map Codex abort/limit warnings to OpenAI codes (e.g., `length`, `content_filter`), include any known stop signals.
4) Model mapping: centralize the alias table (configurable via env/flag) and validate inputs.
5) Error surface: map Codex abort reasons to stable `type` codes; optionally include `code`/`param`.
6) Observability: optional request logging/metrics toggles; maybe a health endpoint.
7) Config: flags/env for sandbox/approval overrides and max concurrent requests.

## Quick how-to-run
- Start: `cargo run -p codex-openai-proxy` (from `codex-rs`).
- Address: `CODEX_OPENAI_PROXY_ADDR=0.0.0.0:11435` (env).
- Auth: reuse Codex auth (e.g., `~/.codex/auth.json`).

## Open items / next iterations
- Expand model mapping: add more aliases (vendor-specific) and a fallback/validation strategy.
- Tool-call mapping:
  - Propagate additional metadata if Codex exposes it (e.g., call arguments structure vs raw string).
  - Decide whether to buffer and emit aggregated tool_calls in stream final chunk (currently send delta chunks only plus finish_reason).
- Finish_reason nuances:
  - If both text and tool calls exist, confirm desired semantics (currently `tool_calls` takes precedence).
  - Consider `length` / `content_filter` codes if Codex can expose them.
- Error surface:
  - Map Codex abort reasons/warnings into OpenAI-compatible error codes where applicable.
  - Add more detailed messages for upstream HTTP failures (if any future HTTP client usage is introduced).
- Responses endpoint parity:
  - `/v1/responses` currently reuses chat handler; if OpenAI Responses has divergent schema, add a dedicated serializer.
- Configurability:
  - Add CLI/env toggles for model mapping table, default sandbox/approval settings, and max concurrent requests.
  - Optional request logging/metrics hooks.

## How to run
- Start: `cargo run -p codex-openai-proxy` (from `codex-rs`).
- Configure addr: `CODEX_OPENAI_PROXY_ADDR=0.0.0.0:11435` (env).
- Requires Codex auth present (e.g., `~/.codex/auth.json`).

## How to test quickly
- Non-stream:  
  `curl -H "Content-Type: application/json" -d '{"model":"gpt-4.1-mini","messages":[{"role":"user","content":"Hello"}],"stream":false}' http://127.0.0.1:11435/v1/chat/completions`
- Stream:  
  `curl -N -H "Content-Type: application/json" -d '{"model":"gpt-4.1-mini","messages":[{"role":"user","content":"Hi"}],"stream":true}' http://127.0.0.1:11435/v1/chat/completions`
- Conversation reuse: include `conversation_id` from a prior response in the next request body.

## Housekeeping
- After code changes: `just fmt` then `cargo check -p codex-openai-proxy` (or `just fix -p codex-openai-proxy` if linters needed).
- If API surface changes, update any relevant docs in `docs/` (none created yet for proxy).
