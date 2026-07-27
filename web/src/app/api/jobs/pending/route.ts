import { NextRequest } from "next/server";
import { db } from "@/db";
import { tlsCommerceJobs } from "@/db/schema";
import { and, asc, eq, sql } from "drizzle-orm";
import { resolveTalosFromRequest } from "@/lib/auth";

// GET /api/jobs/pending — Get pending jobs for the authenticated TALOS (as service provider)
// Requires commerce:read scope (scoped key) or legacy key.
export async function GET(request: NextRequest) {
  try {
    const auth = await resolveTalosFromRequest(request, ["commerce:read"]);
    if (!auth.ok) return auth.response;

    const { searchParams } = new URL(request.url);
    const cursor = searchParams.get("cursor");
    const limit = Math.min(Math.max(parseInt(searchParams.get("limit") ?? "50", 10) || 50, 1), 200);

    const conditions = [
      eq(tlsCommerceJobs.talosId, auth.talos.id),
      eq(tlsCommerceJobs.status, "pending"),
    ];
    if (cursor) conditions.push(sql`${tlsCommerceJobs.createdAt} > ${new Date(cursor)}`);

    const rows = await db
      .select()
      .from(tlsCommerceJobs)
      .where(and(...conditions))
      .orderBy(asc(tlsCommerceJobs.createdAt))
      .limit(limit + 1);

    const hasMore = rows.length > limit;
    const jobs = hasMore ? rows.slice(0, limit) : rows;
    const nextCursor = hasMore ? jobs[jobs.length - 1]?.createdAt.toISOString() ?? null : null;

    return Response.json({ jobs, nextCursor });
  } catch {
    return Response.json({ error: "Internal server error" }, { status: 500 });
  }
}
