import { describe, expect, it, vi, beforeEach } from "vitest";
import { ingestJobToLedger } from "../src/lib/reputation-ledger";
import { db } from "../src/db";

vi.mock("../src/db", () => {
  return {
    db: {
      select: vi.fn().mockReturnThis(),
      from: vi.fn().mockReturnThis(),
      where: vi.fn().mockReturnThis(),
      limit: vi.fn().mockReturnThis(),
      then: vi.fn(),
      insert: vi.fn().mockReturnThis(),
      values: vi.fn().mockReturnThis(),
      onConflictDoUpdate: vi.fn().mockReturnThis(),
      returning: vi.fn(),
    }
  };
});

describe("reputation-ledger", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  describe("ingestJobToLedger", () => {
    it("throws if job not found", async () => {
      vi.mocked(db.then).mockResolvedValueOnce([]);
      await expect(ingestJobToLedger("missing")).rejects.toThrow("Job missing not found");
    });

    it("returns null for non-terminal jobs", async () => {
      vi.mocked(db.then).mockResolvedValueOnce([{ id: "job1", status: "pending" }]);
      const res = await ingestJobToLedger("job1");
      expect(res).toBeNull();
      expect(db.insert).not.toHaveBeenCalled();
    });

    it("ingests a terminal job idempotently", async () => {
      const mockJob = {
        id: "job2",
        status: "completed",
        talosId: "seller1",
        requesterTalosId: "buyer1",
        createdAt: new Date(),
        updatedAt: new Date(),
        txHash: "hash123",
        result: { some: "data" }
      };

      const mockInserted = { ...mockJob, hasResult: true };
      vi.mocked(db.then).mockResolvedValueOnce([mockJob]);
      vi.mocked(db.returning).mockResolvedValueOnce([mockInserted] as any);

      const res = await ingestJobToLedger("job2");
      expect(res).toEqual(mockInserted);
      expect(db.insert).toHaveBeenCalled();
      expect(db.onConflictDoUpdate).toHaveBeenCalled();
    });
  });
});
