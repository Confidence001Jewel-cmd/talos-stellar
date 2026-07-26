# Observability Guide

## Error Tracking (Sentry)

### Web (Next.js)
Errors are auto-captured via `@sentry/nextjs`. Configure by setting:
```
SENTRY_DSN=<your-dsn>
NEXT_PUBLIC_SENTRY_DSN=<your-dsn>
```
in `web/.env.local`. Both vars are needed: `SENTRY_DSN` for server-side routes, `NEXT_PUBLIC_SENTRY_DSN` for client-side.

To verify Sentry is working, add a deliberate throw to any API route:
```ts
throw new Error("Sentry test error");
```
Then check your Sentry dashboard.

### Agent (Python)
Errors are captured via `sentry-sdk` with the asyncio integration. Configure:
```
SENTRY_DSN=<your-dsn>
```
in `packages/prime-agent/.env`. Leave blank to disable.

## Structured Logging

### Web (Next.js) — pino
Logs are emitted as JSON lines in production. Import and use:
```ts
import { logger } from "@/lib/logger";
logger.info({ requestId }, "handler called");
logger.error({ err, requestId }, "handler failed");
```

In development, logs are pretty-printed via `pino-pretty`.

### Agent (Python) — structlog
Logs are JSON lines on stdout, captured by Railway.
```python
import structlog
log = structlog.get_logger(__name__)
log.info("event_name", key="value")
```

Every agent cycle binds a `cycle_id` UUID to the log context via `structlog.contextvars`.

## Request Correlation

### X-Request-Id header
Every web API response includes an `X-Request-Id` header (UUID). When the agent calls the web API, it propagates its `cycle_id` as `X-Request-Id`, so both sides' logs can be correlated:

- Web log: `{ "requestId": "abc-123", ... }`
- Agent log: `{ "cycle_id": "abc-123", ... }`

To cross-reference: filter both log streams by the same ID.

## Where to find logs

| Layer | Where |
|---|---|
| Web errors | Sentry dashboard → `talos-stellar-web` project |
| Web logs | Vercel dashboard → Functions tab → Log drain |
| Agent errors | Sentry dashboard → `talos-stellar-agent` project |
| Agent logs | Railway dashboard → Deployment logs |

## Idempotency Observability

### Structured log events

All idempotency state transitions are logged as structured events. Keys and route paths are
logged; payload contents and response bodies are **never** logged.

#### Web (pino)

| `event` field | When emitted | Log level |
|---|---|---|
| `idempotency_miss` | New key seen for first time | `info` |
| `idempotency_hit` | Cache hit — cached response returned | `info` |
| `idempotency_inflight` | Key exists but response not yet cached | `info` |
| `idempotency_conflict` | Key reused with different payload | `warn` |
| `idempotency_commit` | buy-token purchase committed successfully | `info` |

Example log line (JSON):
```json
{
  "level": "info",
  "time": "2026-07-24T18:00:00.000Z",
  "event": "idempotency_hit",
  "idempotencyKey": "550e8400-e29b-41d4-a716-446655440000",
  "talosId": "abc123",
  "jobId": "job-xyz",
  "replayed": true,
  "msg": "idempotent replay — returning cached response"
}
```

#### Agent (Python / structlog)

| Event | When emitted |
|---|---|
| `idempotency_key_injected` | Key appended to outbound POST/PATCH |
| `idempotency_conflict` | `IdempotencyConflictError` raised |

### Metrics

Aggregate the structured log events with a log drain or query:

| Suggested metric name | `event` filter |
|---|---|
| `idempotency_hit_total` | `event = "idempotency_hit"` |
| `idempotency_miss_total` | `event = "idempotency_miss"` |
| `idempotency_conflict_total` | `event = "idempotency_conflict"` |
| `idempotency_inflight_total` | `event = "idempotency_inflight"` |

### Response headers

Use the response headers to detect replays at the HTTP layer (e.g. in a proxy or test harness):

```
Idempotency-Key: 550e8400-e29b-41d4-a716-446655440000
X-Idempotent-Replayed: true
```

## Pagination

List endpoints now support cursor-based pagination:

| Endpoint | Paginated |
|---|---|
| `GET /api/talos/:id/approvals` | ✅ |
| `GET /api/talos/:id/revenue` | ✅ |
| `GET /api/talos/:id/activity` | ✅ |
| `GET /api/jobs/pending` | ✅ |
| `GET /api/activity` | ✅ (pre-existing) |

### Usage
```
GET /api/talos/:id/approvals?limit=50
GET /api/talos/:id/approvals?limit=50&cursor=2024-01-15T12:00:00.000Z
```

Response shape:
```json
{
  "approvals": [...],
  "nextCursor": "2024-01-14T08:30:00.000Z"
}
```

`nextCursor` is `null` when there are no more pages. Default limit is 50, max is 200.

## Provider Reputation

`GET /api/talos/:id/reputation` returns the versioned provider
reputation score with confidence, decay, and bounded counterparty
influence. See `REPUTATION.md` for the full algorithm contract.

Signals to alert on (Sentry):

- `inputs.evidence === "insufficient"` for an active provider
- `inputsTrace.concentrationDamping < 0.5` for a non-new provider
  (likely sybil / single-buyer pattern)
- `score` is requested but `scoreVersion` returned is not pinned to
  the expected version (cache or formula break)

Cache key for downstream stores: `(talosId, scoreVersion, dayBucket)`.
