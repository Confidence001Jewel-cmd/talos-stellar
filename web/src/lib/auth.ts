import { NextRequest } from "next/server";
import { timingSafeEqual } from "crypto";
import { db } from "@/db";
import { tlsTalos, tlsApiAuditLogs } from "@/db/schema";
import { eq } from "drizzle-orm";
import { enqueue, jobsConfig } from "@/lib/jobs";
import { AUDIT_LOG_WRITE_QUEUE } from "@/lib/jobs/handlers/audit-log";

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
): Promise<
  | { ok: true; talos: { id: string; apiKey: string | null } }
  | { ok: false; response: Response }
> {
  const authHeader = request.headers.get("authorization");

  if (!authHeader || !authHeader.startsWith("Bearer ")) {
    return {
      ok: false,
      response: Response.json(
        { error: "Missing Authorization header. Use: Bearer <api_key>" },
        { status: 401 },
      ),
    };
  }

  const token = authHeader.slice(7);

  const talos = await db
    .select({ id: tlsTalos.id, apiKey: tlsTalos.apiKey })
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

  if (
    !talos.apiKey ||
    talos.apiKey.length !== token.length ||
    !timingSafeEqual(Buffer.from(talos.apiKey), Buffer.from(token))
  ) {
    // Log failed auth attempt (fire-and-forget — never block the response)
    writeAuditLog(talos.id, request, 403).catch(() => {});
    return {
      ok: false,
      response: Response.json({ error: "Invalid API key" }, { status: 403 }),
    };
  }

  // Log successful auth (fire-and-forget)
  writeAuditLog(talos.id, request, 200).catch(() => {});

  return { ok: true, talos };
}

/**
 * Persist one audit log entry. Called fire-and-forget — must not throw.
 *
 * When JOBS_ENABLED=true, this durably enqueues the write instead of
 * inserting directly: a transient DB error is retried with backoff by the
 * job worker rather than silently dropping the audit entry, which is what
 * the plain insert below does today (the caller's `.catch(() => {})`
 * swallows the failure). Default is unchanged — direct insert — so this is
 * purely additive until an operator opts in.
 */
async function writeAuditLog(
  talosId: string,
  request: NextRequest,
  statusCode: number,
): Promise<void> {
  const ip =
    request.headers.get("x-forwarded-for")?.split(",")[0]?.trim() ??
    request.headers.get("x-real-ip") ??
    null;

  const url = new URL(request.url);
  const entry = {
    talosId,
    method: request.method,
    path: url.pathname,
    statusCode,
    ipAddress: ip,
  };

  if (jobsConfig.enabled) {
    await enqueue(AUDIT_LOG_WRITE_QUEUE, entry);
    return;
  }

  await db.insert(tlsApiAuditLogs).values(entry);
}
