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

it("a machine that continues under another row id keeps its skipped target (#119)", async () => {
  const disk: Record<string, unknown> = { [SKIPPED_VERSIONS_SETTING]: { h_aaaaaaaa: "release:1.6.0", h_bbbbbbbb: "release:1.5.0", r_b: "release:1.7.0" } };
  const store = { get: async <T,>(k: string) => disk[k] as T | undefined, set: async (k: string, v: unknown) => { disk[k] = JSON.parse(JSON.stringify(v)); } };
  const skips = new SkippedVersions(async () => store);
  skips.move("h_aaaaaaaa", "r_a");
  skips.move("h_bbbbbbbb", "r_b"); // the Ring row's own skip stays
  skips.move("h_cccccccc", "r_c"); // nothing skipped: nothing written for it
  await new Promise(r => setTimeout(r, 10));
  expect(disk[SKIPPED_VERSIONS_SETTING]).toEqual({ r_a: "release:1.6.0", r_b: "release:1.7.0" });
  expect(skips.getSnapshot().skipped).toEqual({ r_a: "release:1.6.0", r_b: "release:1.7.0" });
});
