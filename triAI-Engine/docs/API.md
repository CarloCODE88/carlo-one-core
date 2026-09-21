# HTTP API

The local server defaults to `127.0.0.1:8900`. A non-loopback bind requires
`server.auth_token` or `TRI_AI_AUTH_TOKEN`; send it as `Authorization: Bearer`
or `X-TRI-Auth`. The server assigns `user_id` from configuration, never from a
request.

| Method | Endpoint | Purpose |
|---|---|---|
| GET | `/health` | `ok` only with a ready model; includes model, VRAM and queue state. |
| GET | `/v1/models` | OpenAI-compatible available-model listing. |
| POST | `/v1/chat/completions` | OpenAI-compatible chat; `stream: true` returns SSE. |
| POST | `/v1/embeddings` | Embeddings proxy to the active worker. |
| POST | `/api/engine/plan` | Validates a supplied model profile and returns a plan id. |
| POST | `/api/engine/plan/auto` | Builds a plan from an indexed model. |
| POST | `/api/engine/start` | Starts only the current plan; single-slot policy applies. |
| POST | `/api/engine/stop` | Stops the active worker. |
| GET | `/api/engine/status` | Supervisor state and active model. |

`max_tokens` is capped at 1200 and rejected before worker contact. Requests
without a ready model, or while the one worker slot is busy, receive explicit
error responses rather than silently starting a second model.
