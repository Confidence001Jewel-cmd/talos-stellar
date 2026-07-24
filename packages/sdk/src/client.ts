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
  CursorPage,
  CursorRequestOptions,
  ActivityPage,
  ActivityPageOptions,
} from "./types.js";
import {
  generateIdempotencyKey,
  validateIdempotencyKey,
  isPayloadConflict,
  IdempotencyConflictError,
} from "./idempotency.js";

export type { IdempotencyConflictError };

export interface RetryPolicyOptions {
  maxAttempts?: number;
  baseDelayMs?: number;
  maxDelayMs?: number;
  retryMethods?: string[];
  retryStatusCodes?: number[];
  jitter?: boolean;
  random?: () => number;
}

export interface TalosClientOptions {
  baseUrl?: string;
  apiKey?: string;
  retryPolicy?: RetryPolicyOptions;
}

/**
 * Per-call options for write methods (POST / PATCH).
 *
 * Supplying an `idempotencyKey` enables safe retry: the same key will be sent
 * on every attempt, allowing the server to de-duplicate the request and return
 * the cached response if the first attempt already committed.
 *
 * When `idempotencyKey` is present, the method is also added to the retry-
 * eligible set for that call (in addition to the standard safe methods like
 * GET/PUT/DELETE), so transient 429/5xx errors trigger automatic retries.
 */
export interface WriteOptions {
  /**
   * Optional idempotency key (UUID v4 recommended). Max 128 bytes.
   * If omitted, the request is sent once with no idempotency guarantee.
   *
   * Use `generateIdempotencyKey()` to create a fresh key, or supply your own
   * stable value to associate retries with a specific logical operation.
   */
  idempotencyKey?: string;
  /** AbortSignal for cancellation. */
  signal?: AbortSignal;
}

export { generateIdempotencyKey, validateIdempotencyKey };

export class TalosClient {
  private baseUrl: string;
  private headers: Record<string, string>;
  private readonly retryPolicy: Required<RetryPolicyOptions>;

  constructor(options: TalosClientOptions = {}) {
    const normalizedRetryMethods = options.retryPolicy?.retryMethods?.map((method) => method.toUpperCase());
    this.retryPolicy = {
      maxAttempts: options.retryPolicy?.maxAttempts ?? 3,
      baseDelayMs: options.retryPolicy?.baseDelayMs ?? 100,
      maxDelayMs: options.retryPolicy?.maxDelayMs ?? 1000,
      retryMethods: normalizedRetryMethods ?? ["GET", "HEAD", "PUT", "DELETE", "OPTIONS"],
      retryStatusCodes: options.retryPolicy?.retryStatusCodes ?? [429, 500, 502, 503, 504],
      jitter: options.retryPolicy?.jitter ?? true,
      random: options.retryPolicy?.random ?? Math.random,
    };
    this.baseUrl = (options.baseUrl ?? "https://talos-stellar.vercel.app").replace(/\/$/, "");
    this.headers = { "Content-Type": "application/json" };
    if (options.apiKey) {
      this.headers["Authorization"] = `Bearer ${options.apiKey}`;
    }
  }

  // ── Internal fetch helper ──────────────────────────────────

  private shouldRetry(method: string, status: number, retryMethodsOverride?: string[]): boolean {
    const methods = retryMethodsOverride ?? this.retryPolicy.retryMethods;
    return (
      this.retryPolicy.retryStatusCodes.includes(status) &&
      methods.includes(method)
    );
  }

  private getRetryDelay(attempt: number, retryAfterHeader: string | null): number {
    if (retryAfterHeader) {
      const headerDelay = this.parseRetryAfter(retryAfterHeader);
      if (headerDelay !== null) {
        return Math.min(headerDelay, this.retryPolicy.maxDelayMs);
      }
    }

    const exponent = Math.pow(2, attempt - 1);
    const delay = Math.min(this.retryPolicy.baseDelayMs * exponent, this.retryPolicy.maxDelayMs);
    if (!this.retryPolicy.jitter) {
      return delay;
    }

    return Math.floor(this.retryPolicy.random() * delay);
  }

  private parseRetryAfter(header: string | null): number | null {
    if (!header) return null;
    const trimmed = header.trim();
    const seconds = Number(trimmed);
    if (!Number.isNaN(seconds)) {
      return Math.max(0, seconds * 1000);
    }

    const parsedDate = Date.parse(trimmed);
    if (!Number.isNaN(parsedDate)) {
      const delta = parsedDate - Date.now();
      return delta > 0 ? delta : 0;
    }

    return null;
  }

  private wait(delayMs: number, signal?: AbortSignal): Promise<void> {
    if (signal?.aborted) {
      return Promise.reject(new Error("Request aborted"));
    }

    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        signal?.removeEventListener("abort", onAbort);
        resolve();
      }, delayMs);

      const onAbort = () => {
        clearTimeout(timeout);
        signal?.removeEventListener("abort", onAbort);
        reject(new Error("Request aborted"));
      };

      signal?.addEventListener("abort", onAbort, { once: true });
    });
  }

  private async request<T>(
    path: string,
    init?: RequestInit & {
      params?: Record<string, string | number | boolean>;
      idempotencyKey?: string;
    },
  ): Promise<T> {
    let url = `${this.baseUrl}${path}`;
    const { params, signal, idempotencyKey, ...requestInit } = init ?? {};
    const normalizedSignal = signal ?? undefined;
    if (params) {
      const filteredParams = Object.entries(params)
        .filter(([_, value]) => value !== undefined)
        .reduce((acc, [key, value]) => ({ ...acc, [key]: String(value) }), {});
      const qs = new URLSearchParams(filteredParams).toString();
      if (qs) url += `?${qs}`;
    }

    const method = (requestInit.method?.toString().toUpperCase() ?? "GET");

    // Build the extra headers for this call
    const extraHeaders: Record<string, string> = {};
    if (idempotencyKey) {
      const validKey = validateIdempotencyKey(idempotencyKey);
      extraHeaders["Idempotency-Key"] = validKey;
    }

    // When an idempotency key is present, POST/PATCH become retry-eligible for
    // this call because the server will de-duplicate if the first attempt committed.
    const retryMethodsForCall =
      idempotencyKey && !this.retryPolicy.retryMethods.includes(method)
        ? [...this.retryPolicy.retryMethods, method]
        : undefined;

    for (let attempt = 1; attempt <= this.retryPolicy.maxAttempts; attempt += 1) {
      const res = await fetch(url, {
        ...requestInit,
        ...(normalizedSignal ? { signal: normalizedSignal } : {}),
        headers: { ...this.headers, ...requestInit.headers, ...extraHeaders },
      });

      if (res.ok) {
        return res.json() as Promise<T>;
      }

      // 409 Conflict — check whether this is a payload conflict (caller error)
      // or an in-flight duplicate (safe to surface as a regular API error).
      if (res.status === 409 && idempotencyKey) {
        const body = await res.text();
        if (isPayloadConflict(body)) {
          throw new IdempotencyConflictError(idempotencyKey, path, body);
        }
        throw new TalosAPIError(409, body, path);
      }

      const shouldRetry = this.shouldRetry(method, res.status, retryMethodsForCall);
      if (!shouldRetry || attempt === this.retryPolicy.maxAttempts) {
        const body = await res.text();
        throw new TalosAPIError(res.status, body, path);
      }

      const retryAfterHeader = res.headers.get("Retry-After");
      const delay = this.getRetryDelay(attempt, retryAfterHeader);
      await this.wait(delay, normalizedSignal);
    }

    throw new Error("Unexpected retry failure");
  }

  private async requestPage<T>(
    path: string,
    options?: CursorRequestOptions,
  ): Promise<CursorPage<T>> {
    const { signal, ...params } = options ?? {};
    return this.request(path, { params, signal });
  }

  // ── Talos CRUD ────────────────────────────────────────────

  async listTaloses(params?: CursorRequestOptions): Promise<CursorPage<Talos>> {
    return this.requestPage("/api/talos", params);
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

  async listActivities(params?: ActivityPageOptions): Promise<ActivityPage> {
    const { signal, ...query } = params ?? {};
    return this.request<ActivityPage>("/api/activity", { params: query, signal });
  }

  async reportActivity(
    talosId: string,
    params: ReportActivityParams,
    options?: WriteOptions,
  ): Promise<Activity> {
    return this.request(`/api/talos/${talosId}/activity`, {
      method: "POST",
      body: JSON.stringify(params),
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
    });
  }

  async getTalosActivities(talosId: string): Promise<Activity[]> {
    return this.request(`/api/talos/${talosId}/activity`);
  }

  // ── Revenue ────────────────────────────────────────────────

  async reportRevenue(
    talosId: string,
    params: ReportRevenueParams,
    options?: WriteOptions,
  ): Promise<Revenue> {
    return this.request(`/api/talos/${talosId}/revenue`, {
      method: "POST",
      body: JSON.stringify(params),
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
    });
  }

  async getTalosRevenues(talosId: string): Promise<Revenue[]> {
    return this.request(`/api/talos/${talosId}/revenue`);
  }

  // ── Approvals ──────────────────────────────────────────────

  async createApproval(
    talosId: string,
    params: CreateApprovalParams,
    options?: WriteOptions,
  ): Promise<Approval> {
    return this.request(`/api/talos/${talosId}/approvals`, {
      method: "POST",
      body: JSON.stringify(params),
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
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

  async discoverServices(params?: DiscoverServicesParams): Promise<CursorPage<CommerceService>> {
    const { signal, ...query } = params ?? {};
    return this.requestPage("/api/services", { ...query, signal });
  }

  async purchaseService(
    talosId: string,
    params: PurchaseServiceParams,
    options?: WriteOptions,
  ): Promise<CommerceJob> {
    return this.request(`/api/talos/${talosId}/service`, {
      method: "POST",
      body: JSON.stringify({ payload: params.payload }),
      headers: { "X-PAYMENT": params.paymentHeader },
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
    });
  }

  /**
   * High-level helper to purchase a service, handling the x402 402 challenge flow.
   *
   * Pass `options.idempotencyKey` to enable safe retry across the entire
   * 402-challenge-and-retry cycle (the key is sent only on the final POST).
   */
  async purchaseServiceWithPayment(
    talosId: string,
    buyerTalosId: string,
    payload?: Record<string, unknown>,
    options?: WriteOptions,
  ): Promise<CommerceJob> {
    let res: Response;
    const url = `${this.baseUrl}/api/talos/${talosId}/service`;

    // 1. Try initial request
    res = await fetch(url, {
      method: "POST",
      headers: this.headers,
      body: JSON.stringify({ payload }),
    });

    if (res.status === 402) {
      // 2. Handle x402 challenge
      const authHeader = res.headers.get("WWW-Authenticate");
      if (!authHeader || !authHeader.startsWith("x402 ")) {
        throw new Error("Invalid x402 challenge");
      }

      // Parse challenge: x402 price="0.50", payee="G...", token="USDC", network="stellar:testnet"
      const challenge = this.parseX402Challenge(authHeader);

      // 3. Request signature from Web API
      const signRes = await this.signPayment(buyerTalosId, {
        payee: challenge.payee,
        amount: parseFloat(challenge.price),
        assetCode: challenge.token,
      });

      // 4. Retry with X-PAYMENT header (and idempotency key if supplied)
      return this.purchaseService(talosId, {
        paymentHeader: signRes.paymentHeader,
        payload,
      }, options);
    }

    if (!res.ok) {
      const body = await res.text();
      throw new TalosAPIError(res.status, body, `/api/talos/${talosId}/service`);
    }

    return res.json() as Promise<CommerceJob>;
  }

  private parseX402Challenge(header: string): Record<string, string> {
    const parts = header.slice(5).split(", ");
    const challenge: Record<string, string> = {};
    for (const part of parts) {
      const [key, value] = part.split("=");
      challenge[key] = value.replace(/"/g, "");
    }
    return challenge;
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

  async transfer(
    talosId: string,
    params: TransferParams,
    options?: WriteOptions,
  ): Promise<TransferResponse> {
    return this.request(`/api/talos/${talosId}/transfer`, {
      method: "POST",
      body: JSON.stringify(params),
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
    });
  }

  // ── Jobs ───────────────────────────────────────────────────

  async getPendingJobs(): Promise<CommerceJob[]> {
    return this.request("/api/jobs/pending");
  }

  /**
   * Submit the result of a fulfilled job.
   *
   * Pass `options.idempotencyKey` to enable safe retry: if the network drops
   * after the server has already committed the result, the retry will receive
   * a 201 from cache rather than creating a duplicate.
   */
  async submitJobResult(
    jobId: string,
    result: unknown,
    options?: WriteOptions,
  ): Promise<CommerceJob> {
    return this.request(`/api/jobs/${jobId}/result`, {
      method: "POST",
      body: JSON.stringify({ result }),
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
    });
  }

  async getJobResult(jobId: string): Promise<CommerceJob> {
    return this.request(`/api/jobs/${jobId}/result`);
  }

  // ── Leaderboard ────────────────────────────────────────────

  async getLeaderboard(params?: CursorRequestOptions): Promise<CursorPage<LeaderboardEntry>> {
    return this.requestPage("/api/leaderboard", params);
  }

  // ── Playbooks ──────────────────────────────────────────────

  async listPlaybooks(params?: {
    category?: string;
    channel?: string;
    search?: string;
  } & CursorRequestOptions): Promise<CursorPage<Playbook>> {
    return this.requestPage("/api/playbooks", params);
  }

  async createPlaybook(
    params: CreatePlaybookParams,
    options?: WriteOptions,
  ): Promise<Playbook> {
    return this.request("/api/playbooks", {
      method: "POST",
      body: JSON.stringify(params),
      idempotencyKey: options?.idempotencyKey,
      signal: options?.signal,
    });
  }
}

export class TalosAPIError extends Error {
  constructor(
    public status: number,
    public body: string,
    public path: string,
  ) {
    super(`Talos API error ${status} on ${path}: ${body}`);
    this.name = "TalosAPIError";
  }
}
