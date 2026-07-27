import { db } from "@/db";
import { tlsCommerceJobs, tlsReputationInputs } from "@/db/schema";
import { eq } from "drizzle-orm";
import type { PgTransaction } from "drizzle-orm/pg-core";
import type { ExtractTablesWithRelations } from "drizzle-orm";
import type { PostgresJsQueryResultHKT } from "drizzle-orm/postgres-js";

// Terminal statuses that should be recorded in the ledger
export const TERMINAL_JOB_STATUSES = [
  "completed",
  "accepted",
  "fulfilled",
  "settled",
  "failed",
  "rejected",
  "cancelled",
  "disputed",
];

// Reusable type for a generic Drizzle postgres transaction
export type DbTx = PgTransaction<
  PostgresJsQueryResultHKT,
  Record<string, never>,
  ExtractTablesWithRelations<Record<string, never>>
>;

/**
 * Idempotently ingests a terminal job into the reputation input ledger.
 * This preserves provenance (txHash, status, counterparties, etc.)
 * without leaking private job payloads or results.
 */
export async function ingestJobToLedger(jobId: string, tx?: any) {
  const dbOrTx = tx ?? db;

  const job = await dbOrTx
    .select()
    .from(tlsCommerceJobs)
    .where(eq(tlsCommerceJobs.id, jobId))
    .limit(1)
    .then((r: any) => r[0] ?? null);

  if (!job) {
    throw new Error(`Job ${jobId} not found`);
  }

  if (!TERMINAL_JOB_STATUSES.includes(job.status)) {
    // We only record settled/terminal outcomes
    return null;
  }

  // A job has a result if the result JSON is not null and not empty
  const hasResult =
    job.result != null &&
    typeof job.result === "object" &&
    Object.keys(job.result).length > 0;

  const [inserted] = await dbOrTx
    .insert(tlsReputationInputs)
    .values({
      talosId: job.talosId,
      jobId: job.id,
      requesterTalosId: job.requesterTalosId,
      status: job.status,
      jobCreatedAt: job.createdAt,
      jobUpdatedAt: job.updatedAt,
      hasResult,
      txHash: job.txHash,
    })
    .onConflictDoUpdate({
      target: tlsReputationInputs.jobId,
      set: {
        status: job.status,
        jobUpdatedAt: job.updatedAt,
        hasResult,
        txHash: job.txHash,
        updatedAt: new Date(),
      },
    })
    .returning();

  return inserted;
}

/**
 * Sweeps all existing commerce jobs for a provider (or all providers)
 * and rebuilds the reputation ledger. Useful for migrations or
 * retroactive state corrections.
 */
export async function rebuildReputationLedger(providerId?: string) {
  let query = db.select().from(tlsCommerceJobs);

  if (providerId) {
    query = query.where(eq(tlsCommerceJobs.talosId, providerId)) as any;
  }

  const jobs = await query;
  const terminalJobs = jobs.filter((j: any) =>
    TERMINAL_JOB_STATUSES.includes(j.status)
  );

  let ingestedCount = 0;
  // Note: For massive scale, this should use a batch insert.
  // We use sequential ingestion here to reuse the idempotency logic
  // and handle potential concurrent modifications safely.
  for (const job of terminalJobs) {
    await ingestJobToLedger(job.id);
    ingestedCount++;
  }

  return { ingestedCount };
}
