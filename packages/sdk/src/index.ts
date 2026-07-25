// ── Client ────────────────────────────────────────────────────────

export { TalosClient } from "./client.js";
export type {
  TalosClientOptions,
  RetryOptions,
  TalosErrorEvent,
} from "./client.js";

// ── Errors (typed hierarchy) ──────────────────────────────────────
//
// Re-export the existing `TalosAPIError` alias so legacy imports keep
// working, then publish the full hierarchy for callers that want to catch
// specific failure modes.
export { TalosAPIError } from "./errors.js";
export type { TalosAPIErrorOptions, TalosErrorCode } from "./errors.js";
export {
  TalosValidationError,
  TalosAuthenticationError,
  TalosForbiddenError,
  TalosNotFoundError,
  TalosConflictError,
  TalosPaymentError,
  TalosRateLimitError,
  TalosServerError,
  TalosServerRetryableError,
  TalosTransportError,
  TalosTimeoutError,
  errorFromResponse,
  classifyTransportError,
  sanitizeBody,
  redactSecrets,
  snapshotHeaders,
  parseRetryAfter,
  parseX402Challenge,
  MAX_BODY_BYTES,
} from "./errors.js";

// ── Domain types ──────────────────────────────────────────────────

export * from "./types.js";

// ── Stellar helpers ───────────────────────────────────────────────

export * from "./stellar.js";
