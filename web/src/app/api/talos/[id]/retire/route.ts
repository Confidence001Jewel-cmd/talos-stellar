import { db } from "@/db";
import { tlsTalos } from "@/db/schema";
import { eq } from "drizzle-orm";
import { NextResponse } from "next/server";

// POST /api/talos/:id/retire - Retire an agent (preserves history, prevents reuse)
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

    if (talos.retiredAt) {
      return NextResponse.json({ error: "TALOS already retired" }, { status: 400 });
    }

    const body = await _request.json();
    const { reason, supersededBy } = body;

    const [retiredTalos] = await db
      .update(tlsTalos)
      .set({
        retiredAt: new Date(),
        retiredReason: reason || "Agent retired",
        supersededBy: supersededBy || null,
        status: "Retired",
        agentOnline: false,
        updatedAt: new Date(),
      })
      .where(eq(tlsTalos.id, id))
      .returning();

    return NextResponse.json({
      id: retiredTalos.id,
      agentName: retiredTalos.agentName,
      retiredAt: retiredTalos.retiredAt,
      retiredReason: retiredTalos.retiredReason,
      supersededBy: retiredTalos.supersededBy,
    });
  } catch (error) {
    console.error("Error retiring TALOS:", error);
    return NextResponse.json({ error: "Internal server error" }, { status: 500 });
  }
}
