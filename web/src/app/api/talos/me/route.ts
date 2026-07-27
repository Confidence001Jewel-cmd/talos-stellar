import { NextRequest } from "next/server";
import { resolveTalosFromRequest } from "@/lib/auth";

// GET /api/talos/me — Resolve TALOS from API key (Bearer token)
// Supports both scoped keys and legacy plaintext keys.
export async function GET(request: NextRequest) {
  try {
    const auth = await resolveTalosFromRequest(request);
    if (!auth.ok) return auth.response;

    return Response.json(auth.talos);
  } catch {
    return Response.json({ error: "Internal server error" }, { status: 500 });
  }
}
