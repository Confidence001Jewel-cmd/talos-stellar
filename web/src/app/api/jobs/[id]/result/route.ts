import { NextRequest } from "next/server";
import { db } from "@/db";
import { tlsCommerceJobs, tlsRevenues, tlsCommerceServices } from "@/db/schema";
import { eq } from "drizzle-orm";
import { resolveTalosFromRequest } from "@/lib/auth";

// POST /api/jobs/:id/result — Submit job result (from service provider agent)
// Requires commerce:write scope.
export async function POST(
  request: NextRequest,
  { params }: { params: Promise<{ id: string }> }
) {
  const { id } = await params;

  try {
    const auth = await resolveTalosFromRequest(request, ["commerce:write"]);
    if (!auth.ok) return auth.response;

    const body = await request.json();
    const { result } = body;

    if (!result) {
      return Response.json({ error: "result is required" }, { status: 400 });
    }

    const job = await db
      .select()
      .from(tlsCommerceJobs)
      .where(eq(tlsCommerceJobs.id, id))
      .limit(1)
      .then((r) => r[0] ?? null);

    if (!job) {
      return Response.json({ error: "Job not found" }, { status: 404 });
    }

    // Only the service provider TALOS can submit results
    if (job.talosId !== auth.talos.id) {
      return Response.json({ error: "Not authorized to fulfill this job" }, { status: 403 });
    }

    // Guard against double-completion before entering the transaction
    if (job.status === "completed") {
      return Response.json({ error: "Job already completed" }, { status: 409 });
    }

    const updated = await db.transaction(async (tx) => {
      const [updatedJob] = await tx
        .update(tlsCommerceJobs)
        .set({
          result,
          status: "completed",
        })
        .where(eq(tlsCommerceJobs.id, id))
        .returning();

      if (!updatedJob) {
        return null;
      }

      const service = await tx
        .select({ currency: tlsCommerceServices.currency })
        .from(tlsCommerceServices)
        .where(eq(tlsCommerceServices.talosId, job.talosId))
        .limit(1)
        .then((r) => r[0] ?? null);

      await tx.insert(tlsRevenues).values({
        talosId: job.talosId,
        amount: job.amount,
        currency: service?.currency ?? "USDC",
        source: "commerce",
        txHash: job.txHash,
      });

      return updatedJob;
    });

    if (!updated) {
      return Response.json({ error: "Job already completed" }, { status: 409 });
    }

    return Response.json(updated);
  } catch {
    return Response.json({ error: "Internal server error" }, { status: 500 });
  }
}

// GET /api/jobs/:id/result — Poll for job result (from requester agent)
// Requires commerce:read scope.
export async function GET(
  request: NextRequest,
  { params }: { params: Promise<{ id: string }> }
) {
  const { id } = await params;

  try {
    const auth = await resolveTalosFromRequest(request, ["commerce:read"]);
    if (!auth.ok) return auth.response;

    const job = await db
      .select()
      .from(tlsCommerceJobs)
      .where(eq(tlsCommerceJobs.id, id))
      .limit(1)
      .then((r) => r[0] ?? null);

    if (!job) {
      return Response.json({ error: "Job not found" }, { status: 404 });
    }

    // Only the provider or requester can view results
    if (job.talosId !== auth.talos.id && job.requesterTalosId !== auth.talos.id) {
      return Response.json({ error: "Not authorized to view this job" }, { status: 403 });
    }

    return Response.json({
      id: job.id,
      status: job.status,
      result: job.result,
      talosId: job.talosId,
      serviceName: job.serviceName,
    });
  } catch {
    return Response.json({ error: "Internal server error" }, { status: 500 });
  }
}
