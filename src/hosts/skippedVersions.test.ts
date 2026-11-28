import { expect, it, vi } from "vitest";
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => undefined, set: async () => {} }) }));
import { SkippedVersions, SKIPPED_VERSIONS_SETTING } from "./skippedVersions";
import type { HostStatus } from "./types";

const offer = (host: string) => ({ host, status: "connected", updateAction: "update", updateSource: "release", updateVersion: "1.6.0", desktopVersion: "1.6.0" }) as unknown as HostStatus;

it("a slow earlier write never replaces a later skip", async () => {
  const disk: Record<string, unknown> = {};
  let releaseFirst!: () => void;
  const first = new Promise<void>(r => { releaseFirst = r; });
  let writes = 0;
  const store = { get: async <T,>(k: string) => disk[k] as T | undefined, set: async (k: string, v: unknown) => { const copy = JSON.parse(JSON.stringify(v)); if (writes++ === 0) await first; disk[k] = copy; } };
  const skips = new SkippedVersions(async () => store);
  skips.skip("r_a", offer("r_a"));
  await new Promise(r => setTimeout(r, 0));
  skips.skip("r_b", offer("r_b"));
  await new Promise(r => setTimeout(r, 0));
  releaseFirst();
  await new Promise(r => setTimeout(r, 10));
  expect(Object.keys(disk[SKIPPED_VERSIONS_SETTING] as object).sort()).toEqual(["r_a", "r_b"]);
});
