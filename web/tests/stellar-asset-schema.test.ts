import { describe, it, expect } from "vitest";
import { stellarAssetSchema, stellarAssetCodeSchema, stellarPublicKeySchema } from "@/lib/schemas";

describe("Stellar Asset Schema", () => {
  describe("stellarAssetCodeSchema", () => {
    it("should accept valid asset codes", () => {
      expect(stellarAssetCodeSchema.safeParse("XLM").success).toBe(true);
      expect(stellarAssetCodeSchema.safeParse("USDC").success).toBe(true);
      expect(stellarAssetCodeSchema.safeParse("ABC123").success).toBe(true);
      expect(stellarAssetCodeSchema.safeParse("A").success).toBe(true);
      expect(stellarAssetCodeSchema.safeParse("123456789012").success).toBe(true); // 12 chars
    });

    it("should reject invalid asset codes", () => {
      expect(stellarAssetCodeSchema.safeParse("").success).toBe(false);
      expect(stellarAssetCodeSchema.safeParse("1234567890123").success).toBe(false); // 13 chars
      expect(stellarAssetCodeSchema.safeParse("asset!code").success).toBe(false);
      expect(stellarAssetCodeSchema.safeParse("asset code").success).toBe(false);
      expect(stellarAssetCodeSchema.safeParse("asset_code").success).toBe(false);
    });
  });

  describe("stellarPublicKeySchema", () => {
    it("should accept valid Stellar public keys", () => {
      expect(stellarPublicKeySchema.safeParse("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5").success).toBe(true);
      expect(stellarPublicKeySchema.safeParse("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN").success).toBe(true);
    });

    it("should reject invalid Stellar public keys", () => {
      expect(stellarPublicKeySchema.safeParse("").success).toBe(false);
      expect(stellarPublicKeySchema.safeParse("invalid").success).toBe(false);
      expect(stellarPublicKeySchema.safeParse("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLAX").success).toBe(false); // Bad checksum
    });
  });

  describe("stellarAssetSchema", () => {
    it("should accept native XLM asset", () => {
      const result = stellarAssetSchema.safeParse({ type: "native" });
      expect(result.success).toBe(true);
    });

    it("should accept valid issued assets", () => {
      const result = stellarAssetSchema.safeParse({
        type: "issued",
        code: "USDC",
        issuer: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
      });
      expect(result.success).toBe(true);
    });

    it("should reject issued assets with invalid code", () => {
      const result = stellarAssetSchema.safeParse({
        type: "issued",
        code: "",
        issuer: "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
      });
      expect(result.success).toBe(false);
    });

    it("should reject issued assets with invalid issuer", () => {
      const result = stellarAssetSchema.safeParse({
        type: "issued",
        code: "USDC",
        issuer: "invalid",
      });
      expect(result.success).toBe(false);
    });

    it("should reject assets with invalid type", () => {
      const result = stellarAssetSchema.safeParse({ type: "invalid" });
      expect(result.success).toBe(false);
    });
  });
});
