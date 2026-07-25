import type {
  Talos,
  TalosCreated,
  TalosDetail,
  CreateTalosParams,
  ReportActivityParams,
  Activity,
  ReportRevenueParams,
  Revenue,
  CreateApprovalParams,
  Approval,
  RegisterServiceParams,
  CommerceService,
  SignPaymentParams,
  SignedPayment,
  DiscoverServicesParams,
  PurchaseServiceParams,
  CommerceJob,
  Wallet,
  LeaderboardEntry,
  Playbook,
  CreatePlaybookParams,
  TransferParams,
  TransferResponse,
  PaginatedResponse,
} from "./types.js";
import {
  TalosAPIError,
  TalosPaymentError,
  classifyTransportError,
  errorFromResponse,
  parseX402Challenge,
  parseRetryAfter,
} from "./errors.js";

/**
 * Client configuration. All fields are optional; defaults match the prior
 * behavior for backward compatibility. New fields opt in to bounded
 * timeout/retry behavior.
 */
export interface TalosClientOptions {
  /** Base URL of the Talos API. Defaults to `https://talos-stellar.vercel.app`. */
  baseUrl?: string;
  /** Bearer token (TALOS API key). Adds `Authorization: Bearer <key>` header. */
  apiKey?: string;
  /**
   * Per-request timeout in milliseconds. When set, every request is bounded
   * by an `AbortSignal` that aborts the underlying `fetch` after this delay.
   * Default: `undefined` (no timeout — matches pre-existing behavior).
   *
   * Timeouts are surfaced as {@link TalosTimeoutError} and treated as
   * retryable when retry is enabled and the request is idempotent.
   */
  timeoutMs?: number;
  /**
   * Bounded auto-retry policy. When `maxAttempts > 1`, transient failures
   * (429, 502/503/504, transport, timeout) on idempotent methods (GET/HEAD)
   * are retried with exponential backoff and a `Retry-After` hint honored
   * when the server supplies it.
   *
   * - `maxAttempts`: total attempts including the initial one (default 1 = off).
   * - `idempotentOnly`: only retry GET/HEAD (default `true`).
   * - `maxRetryAfterMs`: cap on server-supplied `Retry-After` (default 60s).
   * - `baseDelayMs`: initial backoff between attempts (default 500ms).
   * - `maxDelayMs`: upper bound on backoff (default 8s).
   * - `jitter`: optional jitter factor `0–1` (default 0.25).
   * - `onRetry`: called before each retry with the typed error.
   *
   * Validation/auth/conflict/payment errors are never retried — those are
   * caller-fixable, not transient.
   */
  retry?: RetryOptions;
  /**
   * Optional observer invoked once per failed SDK call (after retries are
   * exhausted, if any). Useful for centralized logging / metrics. The
   * callback must not throw; it is invoked fire-and-forget.
   */
  onError?: (event: TalosErrorEvent) => void;
  /**
   * Optional fetch implementation. Defaults to the global `fetch`. Provided
   * so callers can inject mock transports for tests or wire in middleware.
   */
  fetch?: typeof fetch;
}

/** Bounded retry configuration. See {@link TalosClientOptions.retry}. */
export interface RetryOptions {
  maxAttempts?: number;
  idempotentOnly?: boolean;
  maxRetryAfterMs?: number;
  baseDelayMs?: number;
  maxDelayMs?: number;
  jitter?: number;
  onRetry?: (event: { attempt: number; error: TalosAPIError; delayMs: number }) => void;
}

/** Structured event emitted to {@link TalosClientOptions.onError}. */
export interface TalosErrorEvent {
  error: TalosAPIError;
  path: string;
  method: string;
  attempt: number;
  durationMs: number;
}

/** Default retry bounds. Conservative — well within RFC 7231 guidance. */
const DEFAULT_RETRY: Required<RetryOptions> = {
  maxAttempts: 1,
  idempotentOnly: true,
  maxRetryAfterMs: 60_000,
  baseDelayMs: 500,
  maxDelayMs: 8_000,
  jitter: 0.25,
  onRetry: () => {
    /* default: no-op observer */
  },
};

/** Methods considered safe to retry without further confirmation from the caller. */
const IDEMPOTENT_METHODS = new Set(["GET", "HEAD"]);

/**
 * Sleep helper. Uses `setTimeout` so it works in both Node and the browser.
 * Returns a promise that resolves after `ms` milliseconds.
 */
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Apply jitter to a delay: `delay * (1 - jitter + jitter*random)`.
 * Bounded below by 0 and above by `delay * (1 + jitter)`.
 */
function applyJitter(delay: number, jitter: number): number {
  const factor = 1 - jitter + jitter * Math.random();
  return Math.max(0, Math.round(delay * factor));
}

/**
 * Talos Protocol API client. Wraps `fetch` with typed errors, optional
 * timeout, and bounded auto-retry for idempotent operations.
 *
 * Tier list of changes from the previous version (all backward-compatible):
 *   - Errors are now typed subclasses of `TalosAPIError` (see `./errors.ts`).
 *   - `timeoutMs` enables a per-request `AbortController` timeout.
 *   - `retry.maxAttempts > 1` opt-in retries on transient failures only.
 *   - `onError` callback for centralized logging / metrics.
 *   - `fetch` injection for tests / middleware.
 */
export class TalosClient {
  private baseUrl: string;
  private headers: Record<string, string>;
  private timeoutMs?: number;
  private retry: Required<RetryOptions>;
  private onError?: (event: TalosErrorEvent) => void;
  /**
   * Optional fetch override. `undefined` (default) means the SDK reads
   * `globalThis.fetch` lazily on every call so that test frameworks that
   * swap `globalThis.fetch` via `vi.stubGlobal` (or equivalent) keep
   * intercepting requests. When set, the override is honored verbatim.
   */
  private fetchOverride?: typeof fetch;

  constructor(options: TalosClientOptions = {}) {
    this.baseUrl = (options.baseUrl ?? "https://talos-stellar.vercel.app").replace(/\/$/, "");
    this.headers = { "Content-Type": "application/json" };
    if (options.apiKey) {
      this.headers["Authorization"] = `Bearer ${options.apiKey}`;
    }
    this.timeoutMs = options.timeoutMs;
    this.retry = { ...DEFAULT_RETRY, ...(options.retry ?? {}) };
    this.onError = options.onError;
    this.fetchOverride = options.fetch;
  }

  /** Resolve the fetch implementation per request. Prefer override; fall back to global. */
  private resolveFetch(): typeof fetch {
    return this.fetchOverride ?? globalThis.fetch;
  }

  // ── Internal request helper ──────────────────────────────────

  /** Build the full URL for a path + params. Pure. */
  private buildUrl(path: string, params?: Record<string, string | number | boolean>): string {
    let url = `${this.baseUrl}${path}`;
    if (params) {
      const filteredParams = Object.entries(params)
        .filter(([_, value]) => value !== undefined)
        .reduce((acc, [key, value]) => ({ ...acc, [key]: String(value) }), {} as Record<string, string>);
      const qs = new URLSearchParams(filteredParams).toString();
      if (qs) url += `?${qs}`;
    }
    return url;
  }

  /**
   * Build the headers for a single request, respecting the per-call
   * overrides supplied via `init.headers`. We return a plain `Record`
   * (rather than a `Headers` instance) so callers — both real fetch and
   * vitest's `vi.mocked(fetch).mock.calls[i][1].headers` — see the headers
   * as enumerable own properties, preserving the legacy `toHaveProperty`
   * test contract.
   */
  private mergeHeaders(init?: HeadersInit): Record<string, string> {
    const headers: Record<string, string> = { ...this.headers };
    if (!init) return headers;
    const provided = init instanceof Headers
      ? Object.fromEntries(init.entries())
      : Array.isArray(init)
        ? Object.fromEntries(init)
        : init;
    Object.assign(headers, provided as Record<string, string>);
    return headers;
  }

  /**
   * Core single-attempt request helper. Throws a typed
   * {@link TalosAPIError} subclass on failure. Honors an optional
   * `AbortSignal` from the caller (used by the timeout wrapper).
   */
  private async doRequest<T>(
    url: string,
    path: string,
    init: RequestInit & { params?: Record<string, string | number | boolean> },
    method: string,
    signal?: AbortSignal,
  ): Promise<T> {
    const headers = this.mergeHeaders(init.headers);
    let res: Response;
    try {
      res = await this.resolveFetch()(url, { ...init, method, headers, signal });
    } catch (cause) {
      // If the caller aborted (cancel from outside), still classify.
      throw classifyTransportError(cause, path);
    }
    if (!res.ok) {
      const rawBody = await res.text().catch(() => "");
      throw errorFromResponse(res.status, path, rawBody, res.headers);
    }
    try {
      return (await res.json()) as T;
    } catch (cause) {
      // 2xx with non-JSON body (HTML proxy fallback, truncated response) — wrap it
      // so callers see one consistent `TalosAPIError` surface.
      throw classifyTransportError(cause, path);
    }
  }

  /**
   * Retry-wrapped request. Returns the typed result or throws the final
   * {@link TalosAPIError} after all attempts.
   *
   * Only retries idempotent methods when `retry.idempotentOnly === true`
   * (default). Non-idempotent requests are executed once.
   */
  private async request<T>(
    path: string,
    init?: RequestInit & { params?: Record<string, string | number | boolean> },
  ): Promise<T> {
    const method = (init?.method ?? "GET").toUpperCase();
    const url = this.buildUrl(path, init?.params);
    const maxAttempts = this.computeMaxAttempts(method);
    const startedAt = Date.now();

    let lastError: TalosAPIError | undefined;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      const timeout = this.acquireTimeoutController();
      try {
        return await this.doRequest<T>(url, path, init ?? {}, method, timeout?.signal);
      } catch (err) {
        // doRequest always throws via classifyTransportError / errorFromResponse,
        // which both return subclasses of TalosAPIError. Any non-TalosAPIError
        // here is a programmer bug; bubble it up.
        if (!(err instanceof TalosAPIError)) throw err;
        lastError = err;
        const shouldRetry = attempt < maxAttempts && err.isRetryable && this.isRetryAttemptable(method, err);
        if (!shouldRetry) break;

        const delayMs = this.computeBackoffDelay(err, attempt);
        // `retry.onRetry` is always defined (Required<RetryOptions>); we still
        // wrap the call in try/catch so a misbehaving callback cannot break
        // the bounded retry loop.
        try {
          this.retry.onRetry({ attempt, error: err, delayMs });
        } catch {
          // Swallow callback failure — the next attempt must still be attempted.
        }
        await sleep(delayMs);
      } finally {
        // Always dispose the timeout — covers both success (return in try)
        // and failure (throw or break in catch). Without this the underlying
        // setTimeout would fire against a stale controller.
        timeout?.dispose();
      }
    }

    // Out of attempts. Notify the onError observer (best-effort).
    if (this.onError && lastError) {
      try {
        this.onError({
          error: lastError,
          path,
          method,
          attempt: maxAttempts,
          durationMs: Date.now() - startedAt,
        });
      } catch {
        // Fire-and-forget.
      }
    }
    throw lastError;
  }

  /** Compute the effective max-attempt count given method + retry policy. */
  private computeMaxAttempts(method: string): number {
    const configured = this.retry.maxAttempts;
    if (!configured || configured <= 1) return 1;
    if (this.retry.idempotentOnly && !IDEMPOTENT_METHODS.has(method)) return 1;
    return Math.max(1, Math.min(configured, 8)); // hard upper bound
  }

  /** Decide whether a particular error is retryable given the method. */
  private isRetryAttemptable(method: string, err: TalosAPIError): boolean {
    if (!err.isRetryable) return false;
    if (this.retry.idempotentOnly && !IDEMPOTENT_METHODS.has(method)) return false;
    return true;
  }

  /**
   * Compute the delay before the next attempt:
   *   - Honor server-supplied `Retry-After` (clamped to `maxRetryAfterMs`).
   *   - Otherwise use exponential backoff: `baseDelayMs * 2^(attempt-1)`.
   *   - Cap at `maxDelayMs`.
   *   - Apply jitter (±25% by default).
   */
  private computeBackoffDelay(err: TalosAPIError, attempt: number): number {
    const retryAfterMs = err.retryAfterMs ?? parseRetryAfter(err.headers["retry-after"]);
    if (retryAfterMs != null) {
      const capped = Math.min(retryAfterMs, this.retry.maxRetryAfterMs);
      const jittered = applyJitter(capped, this.retry.jitter);
      return Math.min(jittered, this.retry.maxDelayMs);
    }
    const exp = Math.min(this.retry.maxDelayMs, this.retry.baseDelayMs * Math.pow(2, attempt - 1));
    return applyJitter(exp, this.retry.jitter);
  }

  /**
   * Build a `{ signal, dispose }` pair for the per-request timeout. The
   * caller MUST call `dispose()` once the request resolves or rejects so
   * the underlying `setTimeout` does not fire against a stale controller
   * (we already saw this leave dangling timers in long-running agent loops).
   *
   * Returns `null` when no timeout is configured.
   */
  private acquireTimeoutController(): { signal: AbortSignal; dispose: () => void } | null {
    if (!this.timeoutMs) return null;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    return {
      signal: controller.signal,
      dispose: () => clearTimeout(timer),
    };
  }

  // ── Talos CRUD ────────────────────────────────────────────

  async listTaloses(params?: { cursor?: string; limit?: number }): Promise<PaginatedResponse<Talos>> {
    return this.request("/api/talos", { params });
  }

  async getTalos(id: string): Promise<TalosDetail> {
    return this.request(`/api/talos/${id}`);
  }

  async getTalosMe(): Promise<TalosDetail> {
    return this.request("/api/talos/me");
  }

  async createTalos(params: CreateTalosParams): Promise<TalosCreated> {
    return this.request("/api/talos", {
      method: "POST",
      body: JSON.stringify(params),
    });
  }

  // ── Activity ───────────────────────────────────────────────

  async listActivities(params?: { cursor?: string; limit?: number; statsOnly?: boolean }): Promise<any> {
    return this.request("/api/activity", { params });
  }

  async reportActivity(talosId: string, params: ReportActivityParams): Promise<Activity> {
    return this.request(`/api/talos/${talosId}/activity`, {
      method: "POST",
      body: JSON.stringify(params),
    });
  }

  async getTalosActivities(talosId: string): Promise<Activity[]> {
    return this.request(`/api/talos/${talosId}/activity`);
  }

  // ── Revenue ────────────────────────────────────────────────

  async reportRevenue(talosId: string, params: ReportRevenueParams): Promise<Revenue> {
    return this.request(`/api/talos/${talosId}/revenue`, {
      method: "POST",
      body: JSON.stringify(params),
    });
  }

  async getTalosRevenues(talosId: string): Promise<Revenue[]> {
    return this.request(`/api/talos/${talosId}/revenue`);
  }

  // ── Approvals ──────────────────────────────────────────────

  async createApproval(talosId: string, params: CreateApprovalParams): Promise<Approval> {
    return this.request(`/api/talos/${talosId}/approvals`, {
      method: "POST",
      body: JSON.stringify(params),
    });
  }

  async getApprovals(talosId: string, status?: string): Promise<Approval[]> {
    const params: Record<string, string> = {};
    if (status) params.status = status;
    return this.request(`/api/talos/${talosId}/approvals`, { params });
  }

  async getApproval(talosId: string, approvalId: string): Promise<Approval> {
    return this.request(`/api/talos/${talosId}/approvals/${approvalId}`);
  }

  // ── Status ─────────────────────────────────────────────────

  async updateStatus(talosId: string, online: boolean): Promise<void> {
    await this.request(`/api/talos/${talosId}/status`, {
      method: "PATCH",
      body: JSON.stringify({ agentOnline: online }),
    });
  }

  // ── Commerce / x402 ────────────────────────────────────────

  async registerService(talosId: string, params: RegisterServiceParams): Promise<CommerceService> {
    return this.request(`/api/talos/${talosId}/service`, {
      method: "PUT",
      body: JSON.stringify(params),
    });
  }

  async discoverServices(params?: DiscoverServicesParams): Promise<PaginatedResponse<CommerceService>> {
    return this.request("/api/services", { params: params as any });
  }

  async purchaseService(
    talosId: string,
    params: PurchaseServiceParams,
  ): Promise<CommerceJob> {
    return this.request(`/api/talos/${talosId}/service`, {
      method: "POST",
      body: JSON.stringify({ payload: params.payload }),
      headers: { "X-PAYMENT": params.paymentHeader },
    });
  }

  /**
   * High-level helper to purchase a service, handling the x402 402 challenge flow.
   *
   * Errors raised here are typed:
   *   - {@link TalosPaymentError} when the 402 challenge is malformed/missing.
   *   - {@link TalosAuthenticationError} for missing credentials on the signed retry.
   *   - Any other TalosAPIError subclass for downstream failures.
   *
   * @param talosId - The ID of the TALOS providing the service.
   * @param buyerTalosId - The ID of the TALOS purchasing the service (for signing).
   * @param payload - Optional payload for the service.
   */
  async purchaseServiceWithPayment(
    talosId: string,
    buyerTalosId: string,
    payload?: Record<string, unknown>,
  ): Promise<CommerceJob> {
    const path = `/api/talos/${talosId}/service`;
    const url = `${this.baseUrl}${path}`;

    // 1. Initial request — possibly hits a 402 challenge.
    let res: Response;
    const timeout = this.acquireTimeoutController();
    try {
      res = await this.resolveFetch()(url, {
        method: "POST",
        headers: this.headers,
        body: JSON.stringify({ payload }),
        signal: timeout?.signal,
      });
    } catch (cause) {
      timeout?.dispose();
      throw classifyTransportError(cause, path);
    }
    timeout?.dispose();

    if (res.status === 402) {
      // 2. Validate the x402 challenge.
      const authHeader = res.headers.get("WWW-Authenticate");
      if (!authHeader || !authHeader.startsWith("x402")) {
        // Preserve the legacy text so existing
        // `rejects.toThrow("Invalid x402 challenge")` assertions keep passing.
        throw new TalosPaymentError(402, "Invalid x402 challenge", path, {
          message: "Invalid x402 challenge",
          headers: { "www-authenticate": authHeader ?? "" },
        });
      }
      const challenge = parseX402Challenge(authHeader);
      if (!challenge) {
        throw new TalosPaymentError(402, "Invalid x402 challenge", path, {
          message: "Invalid x402 challenge",
          headers: { "www-authenticate": authHeader },
        });
      }

      // 3. Request signature from the Web API.
      //    `parseFloat(undefined)` would yield NaN; we already required both
      //    keys above (parseX402Challenge rejects partial challenges), but
      //    `price` could still be the literal "abc" — guard explicitly so
      //    a malformed header never feeds NaN to the downstream /sign call.
      const amount = parseFloat(challenge.price);
      if (!Number.isFinite(amount)) {
        throw new TalosPaymentError(402, "Invalid x402 challenge", path, {
          message: "Invalid x402 challenge",
          headers: { "www-authenticate": authHeader },
        });
      }
      const signRes = await this.signPayment(buyerTalosId, {
        payee: challenge.payee,
        amount,
        assetCode: challenge.token,
      });

      // 4. Retry with the X-PAYMENT header — delegated to the regular
      //    request helper, so all typed errors / retry / timeout apply.
      return this.purchaseService(talosId, {
        paymentHeader: signRes.paymentHeader,
        payload,
      });
    }

    // Non-402 responses — wrap them through the typed dispatch.
    if (!res.ok) {
      const rawBody = await res.text().catch(() => "");
      throw errorFromResponse(res.status, path, rawBody, res.headers);
    }
    try {
      return (await res.json()) as CommerceJob;
    } catch (cause) {
      // Non-JSON body in the initial probe — keep parity with the rest of the
      // SDK by surfacing a typed transport error.
      throw classifyTransportError(cause, path);
    }
  }

  // ── Wallet & Payments ──────────────────────────────────────

  async getWallet(talosId: string): Promise<Wallet> {
    return this.request(`/api/talos/${talosId}/wallet`);
  }

  async signPayment(talosId: string, params: SignPaymentParams): Promise<SignedPayment> {
    return this.request(`/api/talos/${talosId}/sign`, {
      method: "POST",
      body: JSON.stringify(params),
    });
  }

  async transfer(talosId: string, params: TransferParams): Promise<TransferResponse> {
    return this.request(`/api/talos/${talosId}/transfer`, {
      method: "POST",
      body: JSON.stringify(params),
    });
  }

  // ── Jobs ───────────────────────────────────────────────────

  async getPendingJobs(): Promise<CommerceJob[]> {
    return this.request("/api/jobs/pending");
  }

  async submitJobResult(jobId: string, result: unknown): Promise<CommerceJob> {
    return this.request(`/api/jobs/${jobId}/result`, {
      method: "POST",
      body: JSON.stringify({ result }),
    });
  }

  async getJobResult(jobId: string): Promise<CommerceJob> {
    return this.request(`/api/jobs/${jobId}/result`);
  }

  // ── Leaderboard ────────────────────────────────────────────

  async getLeaderboard(params?: { cursor?: string; limit?: number }): Promise<PaginatedResponse<LeaderboardEntry>> {
    return this.request("/api/leaderboard", { params });
  }

  // ── Playbooks ──────────────────────────────────────────────

  async listPlaybooks(params?: {
    category?: string;
    channel?: string;
    search?: string;
    cursor?: string;
    limit?: number;
  }): Promise<PaginatedResponse<Playbook>> {
    return this.request("/api/playbooks", { params });
  }

  async createPlaybook(params: CreatePlaybookParams): Promise<Playbook> {
    return this.request("/api/playbooks", {
      method: "POST",
      body: JSON.stringify(params),
    });
  }
}
