import { NextRequest } from "next/server";
import { createHash, randomBytes, timingSafeEqual } from "crypto";
import { db } from "@/db";
import { tlsTalos, tlsApiKeys, tlsApiAuditLogs } from "@/db/schema";
import { eq, and, sql } from "drizzle-orm";
import { logger } from "@/lib/logger";

export const VALID_SCOPES = [
  "admin",
  "activity:write",
  "commerce:read",
  "commerce:write",
  "wallet:read",
  "wallet:sign",
  "settings:read",
  "settings:write",
  "revenue:read",
  "revenue:write",
] as const;

export type Scope = (typeof VALID_SCOPES)[number];

/**
 * Hash an API key for storage or comparison.
 */
export function hashApiKey(key: string): string {
  return createHash("sha256").update(key).digest("hex");
}

/**
 * Generate a new scoped API key.
 * Returns the raw key (shown once) and its SHA-256 hash (stored in DB).
 */
export function generateApiKey(): { raw: string; hash: string } {
  const raw = `tak_${randomBytes(32).toString("hex")}`;
  return { raw, hash: hashApiKey(raw) };
}

/**
 * Extract the Bearer token from the Authorization header.
 * Returns null if missing or malformed.
 */
function extractBearerToken(request: NextRequest): string | null {
  const authHeader = request.headers.get("authorization");
  if (!authHeader || !authHeader.startsWith("Bearer ")) return null;
  return authHeader.slice(7);
}

/**
 * Verify API key from Authorization header against the TALOS's stored key.
 * Returns the talos record if valid, or a Response error to return early.
 *
 * All authenticated requests are logged to tls_api_audit_logs for security
 * hardening (key rotation auditing, anomaly detection, scope tracking).
 */
export async function verifyAgentApiKey(
  request: NextRequest,
  talosId: string,
  requiredScopes: Scope[] = [],
): Promise<
  | { ok: true; talos: { id: string } }
  | { ok: false; response: Response }
> {
  const token = extractBearerToken(request);

  if (!token) {
    return {
      ok: false,
      response: Response.json(
        { error: "Missing Authorization header. Use: Bearer <api_key>" },
        { status: 401 },
      ),
    };
  }

  const tokenHash = hashApiKey(token);

  const talos = await db
    .select({ id: tlsTalos.id, legacyApiKey: tlsTalos.apiKey })
    .from(tlsTalos)
    .where(eq(tlsTalos.id, talosId))
    .limit(1)
    .then((r) => r[0] ?? null);

  if (!talos) {
    return {
      ok: false,
      response: Response.json({ error: "TALOS not found" }, { status: 404 }),
    };
  }

  // 1. Try to match a scoped key
  const scopedKey = await db
    .select({ id: tlsApiKeys.id, scopes: tlsApiKeys.scopes, expiresAt: tlsApiKeys.expiresAt })
    .from(tlsApiKeys)
    .where(
      and(
        eq(tlsApiKeys.talosId, talosId),
        eq(tlsApiKeys.keyHash, tokenHash),
        eq(tlsApiKeys.status, "active")
      )
    )
    .limit(1)
    .then((r) => r[0] ?? null);

  let authorized = false;
  let hasRequiredScopes = false;

  if (scopedKey) {
    // Check expiry
    if (scopedKey.expiresAt && scopedKey.expiresAt < new Date()) {
      logger.warn({ talosId, keyId: scopedKey.id }, "auth.key.expired");
      writeAuditLog(talos.id, request, 403, "expired_key", requiredScopes).catch(() => {});
      return {
        ok: false,
        response: Response.json({ error: "API key has expired" }, { status: 403 }),
      };
    }

    authorized = true;
    // Update lastUsedAt in the background
    db.update(tlsApiKeys)
      .set({ lastUsedAt: new Date() })
      .where(eq(tlsApiKeys.id, scopedKey.id))
      .execute()
      .catch(() => {});

    hasRequiredScopes = requiredScopes.every(
      (scope) =>
        scopedKey.scopes.includes(scope) || scopedKey.scopes.includes("admin")
    );

    logger.info({ talosId, keyId: scopedKey.id, path: new URL(request.url).pathname }, "auth.key.resolved");
  } else if (talos.legacyApiKey) {
    // 2. Fallback to legacy API key
    if (
      talos.legacyApiKey.length === token.length &&
      timingSafeEqual(Buffer.from(talos.legacyApiKey), Buffer.from(token))
    ) {
      authorized = true;
      // Legacy keys are granted all scopes (admin equivalent) for backward compatibility
      hasRequiredScopes = true;

      logger.info({ talosId, path: new URL(request.url).pathname }, "auth.key.resolved (legacy)");
    }
  }

  if (!authorized) {
    logger.warn({ talosId, path: new URL(request.url).pathname }, "auth.key.denied");
    writeAuditLog(talos.id, request, 403, "invalid_key", requiredScopes).catch(() => {});
    return {
      ok: false,
      response: Response.json({ error: "Invalid API key" }, { status: 403 }),
    };
  }

  if (!hasRequiredScopes) {
    logger.warn({ talosId, requiredScopes, path: new URL(request.url).pathname }, "auth.scope.denied");
    writeAuditLog(talos.id, request, 403, "insufficient_scopes", requiredScopes).catch(() => {});
    return {
      ok: false,
      response: Response.json({ error: "Insufficient scopes", required: requiredScopes }, { status: 403 }),
    };
  }

  writeAuditLog(talos.id, request, 200).catch(() => {});

  return { ok: true, talos: { id: talos.id } };
}

/**
 * Resolve a TALOS from a Bearer token without requiring a known talosId.
 * Used by routes like /api/talos/me, /api/jobs/pending, and /api/playbooks
 * where the caller's identity is determined by the key, not the URL.
 *
 * Returns the full TALOS record (minus apiKey) if valid, or a Response error.
 */
export async function resolveTalosFromRequest(
  request: NextRequest,
  requiredScopes: Scope[] = [],
): Promise<
  | { ok: true; talos: { id: string; [key: string]: unknown } }
  | { ok: false; response: Response }
> {
  const token = extractBearerToken(request);

  if (!token) {
    return {
      ok: false,
      response: Response.json(
        { error: "Missing Authorization header. Use: Bearer <api_key>" },
        { status: 401 },
      ),
    };
  }

  const tokenHash = hashApiKey(token);

  // 1. Try scoped key lookup
  const scopedKeyMatch = await db
    .select({ id: tlsApiKeys.id, talosId: tlsApiKeys.talosId, scopes: tlsApiKeys.scopes, expiresAt: tlsApiKeys.expiresAt })
    .from(tlsApiKeys)
    .where(
      and(
        eq(tlsApiKeys.keyHash, tokenHash),
        eq(tlsApiKeys.status, "active")
      )
    )
    .limit(1)
    .then((r) => r[0] ?? null);

  if (scopedKeyMatch) {
    // Check expiry
    if (scopedKeyMatch.expiresAt && scopedKeyMatch.expiresAt < new Date()) {
      logger.warn({ talosId: scopedKeyMatch.talosId, keyId: scopedKeyMatch.id }, "auth.key.expired");
      return {
        ok: false,
        response: Response.json({ error: "API key has expired" }, { status: 403 }),
      };
    }

    // Check scopes
    if (requiredScopes.length > 0) {
      const hasScopes = requiredScopes.every(
        (scope) =>
          scopedKeyMatch.scopes.includes(scope) || scopedKeyMatch.scopes.includes("admin")
      );
      if (!hasScopes) {
        logger.warn({ talosId: scopedKeyMatch.talosId, requiredScopes }, "auth.scope.denied");
        return {
          ok: false,
          response: Response.json({ error: "Insufficient scopes", required: requiredScopes }, { status: 403 }),
        };
      }
    }

    // Update lastUsedAt in background
    db.update(tlsApiKeys)
      .set({ lastUsedAt: new Date() })
      .where(eq(tlsApiKeys.id, scopedKeyMatch.id))
      .execute()
      .catch(() => {});

    // Fetch full TALOS record
    const talos = await db
      .select()
      .from(tlsTalos)
      .where(eq(tlsTalos.id, scopedKeyMatch.talosId))
      .limit(1)
      .then((r) => r[0] ?? null);

    if (!talos) {
      return {
        ok: false,
        response: Response.json({ error: "TALOS not found" }, { status: 404 }),
      };
    }

    const { apiKey: _key, ...safeTalos } = talos;
    writeAuditLog(talos.id, request, 200, undefined, undefined).catch(() => {});
    logger.info({ talosId: talos.id, keyId: scopedKeyMatch.id, path: new URL(request.url).pathname }, "auth.key.resolved");

    return { ok: true, talos: safeTalos };
  }

  // 2. Fallback to legacy plaintext key
  const allTalos = await db
    .select()
    .from(tlsTalos)
    .where(sql`"apiKey" IS NOT NULL`)
    .then((rows) => rows);

  for (const row of allTalos) {
    if (
      row.apiKey &&
      row.apiKey.length === token.length &&
      timingSafeEqual(Buffer.from(row.apiKey), Buffer.from(token))
    ) {
      // Legacy keys get admin-equivalent scopes
      if (requiredScopes.length > 0) {
        // Legacy keys always pass scope check (admin equivalent)
      }

      const { apiKey: _key, ...safeTalos } = row;
      writeAuditLog(row.id, request, 200).catch(() => {});
      logger.info({ talosId: row.id, path: new URL(request.url).pathname }, "auth.key.resolved (legacy)");

      return { ok: true, talos: safeTalos };
    }
  }

  return {
    ok: false,
    response: Response.json({ error: "Invalid API key" }, { status: 403 }),
  };
}

/**
 * Convenience wrapper: authenticate and return early with the error Response.
 * Eliminates the duplicated `if (!auth.ok) return auth.response` pattern.
 */
export async function requireAgentAuth(
  request: NextRequest,
  talosId: string,
  requiredScopes: Scope[] = [],
): Promise<
  | { ok: true; talos: { id: string } }
  | { ok: false; response: Response }
> {
  return verifyAgentApiKey(request, talosId, requiredScopes);
}

/** Persist one audit log entry. Called fire-and-forget — must not throw. */
async function writeAuditLog(
  talosId: string,
  request: NextRequest,
  statusCode: number,
  denialReason?: string,
  scopesRequired?: Scope[],
): Promise<void> {
  const ip =
    request.headers.get("x-forwarded-for")?.split(",")[0]?.trim() ??
    request.headers.get("x-real-ip") ??
    null;

  const url = new URL(request.url);

  await db.insert(tlsApiAuditLogs).values({
    talosId,
    method: request.method,
    path: url.pathname,
    statusCode,
    ipAddress: ip,
    denialReason,
    scopesRequired,
  });
}
