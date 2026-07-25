import { db } from "@/db";
import { tlsTalos, tlsPatrons, tlsActivities, tlsApprovals, tlsRevenues, tlsDividends, tlsCommerceJobs, tlsCommerceServices, tlsPlaybooks, tlsApiAuditLogs } from "@/db/schema";
import { eq } from "drizzle-orm";
import { NextResponse } from "next/server";

// POST /api/talos/:id/delete - Privacy deletion (soft delete, preserves historical links)
// This separates identity retirement from privacy deletion
export async function POST(
  _request: Request,
  { params }: { params: Promise<{ id: string }> }
) {
  const { id } = await params;

  try {
    const talos = await db.query.tlsTalos.findFirst({
      where: eq(tlsTalos.id, id),
    });

    if (!talos) {
      return NextResponse.json({ error: "TALOS not found" }, { status: 404 });
    }

    if (talos.deletedAt) {
      return NextResponse.json({ error: "TALOS already deleted" }, { status: 400 });
    }

    const body = await _request.json();
    const { reason } = body;

    // Soft delete the agent record (preserves ID and historical links)
    const [deletedTalos] = await db
      .update(tlsTalos)
      .set({
        deletedAt: new Date(),
        deletedReason: reason || "Privacy deletion requested",
        // Clear sensitive fields but preserve identity
        apiKey: null,
        agentWalletId: null,
        agentWalletAddress: null,
        walletPublicKey: null,
        creatorPublicKey: null,
        investorPublicKey: null,
        treasuryPublicKey: null,
        updatedAt: new Date(),
      })
      .where(eq(tlsTalos.id, id))
      .returning();

    // Mark related records as deleted (soft delete for historical preservation)
    await db
      .update(tlsPatrons)
      .set({ status: "deleted", updatedAt: new Date() })
      .where(eq(tlsPatrons.talosId, id));

    await db
      .update(tlsActivities)
      .set({ status: "deleted" })
      .where(eq(tlsActivities.talosId, id));

    await db
      .update(tlsApprovals)
      .set({ status: "deleted", updatedAt: new Date() })
      .where(eq(tlsApprovals.talosId, id));

    await db
      .update(tlsCommerceServices)
      .set({ updatedAt: new Date() })
      .where(eq(tlsCommerceServices.talosId, id));

    await db
      .update(tlsCommerceJobs)
      .set({ status: "deleted", updatedAt: new Date() })
      .where(eq(tlsCommerceJobs.talosId, id));

    await db
      .update(tlsPlaybooks)
      .set({ status: "deleted", updatedAt: new Date() })
      .where(eq(tlsPlaybooks.talosId, id));

    return NextResponse.json({
      id: deletedTalos.id,
      agentName: deletedTalos.agentName,
      deletedAt: deletedTalos.deletedAt,
      deletedReason: deletedTalos.deletedReason,
      message: "Agent soft-deleted. Historical records preserved.",
    });
  } catch (error) {
    console.error("Error deleting TALOS:", error);
    return NextResponse.json({ error: "Internal server error" }, { status: 500 });
  }
}
