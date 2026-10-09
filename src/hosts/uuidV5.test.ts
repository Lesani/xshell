import { describe, expect, it } from "vitest";
import { MIGRATION_NS, uuidV5 } from "./uuidV5";

const DNS_NS = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

describe("uuidV5", () => {
  it("matches the RFC vector (python uuid.uuid5(NAMESPACE_DNS, 'www.example.com'))", async () => {
    expect(await uuidV5("www.example.com", DNS_NS)).toBe("2ed6657d-e927-568b-95e1-2665a8aea6a2");
  });

  it("is deterministic, version 5, RFC variant, and depends on the name", async () => {
    const a = await uuidV5("terminal-s1-abc", MIGRATION_NS);
    expect(await uuidV5("terminal-s1-abc", MIGRATION_NS)).toBe(a);
    expect(a).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(await uuidV5("terminal-s2-abc", MIGRATION_NS)).not.toBe(a);
  });

  it("rejects a malformed namespace", async () => {
    await expect(uuidV5("x", "nope")).rejects.toThrow();
  });
});
