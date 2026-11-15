// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useUpdateCheck, type UpdateInfo } from "./useUpdateCheck";

const native = vi.hoisted(() => ({ enabled: true as unknown, check: vi.fn(), fetch: vi.fn() }));
vi.mock("@tauri-apps/api/app", () => ({ getVersion: async () => "1.6.0" }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (command: string) => {
    if (command === "app_updates_enabled") {
      if (native.enabled instanceof Error) throw native.enabled;
      return native.enabled;
    }
    throw new Error(`unexpected ${command}`);
  },
}));
vi.mock("@tauri-apps/plugin-updater", () => ({ check: (...a: unknown[]) => native.check(...a) }));

let container: HTMLDivElement;
let root: Root;
let info: UpdateInfo | null = null;
function Probe() {
  info = useUpdateCheck();
  return null;
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  native.check.mockReset();
  native.check.mockResolvedValue(null);
  native.fetch.mockReset();
  native.fetch.mockResolvedValue({ ok: false });
  vi.stubGlobal("fetch", native.fetch);
  info = null;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});

const mount = async () => { await act(async () => { root.render(<Probe />); }); };

it("a build with the updater off checks nothing and says so", async () => {
  native.enabled = false;
  await mount();
  expect(info?.disabled).toBe(true);
  expect(info?.loading).toBe(false);
  expect(info?.updateAvailable).toBe(false);
  expect(native.check).not.toHaveBeenCalled();
  expect(native.fetch).not.toHaveBeenCalled();
});

it("a build with the updater on checks as before", async () => {
  native.enabled = true;
  await mount();
  expect(info?.disabled).toBe(false);
  expect(native.check).toHaveBeenCalledTimes(1);
});

it("an older backend without the command keeps checking", async () => {
  native.enabled = new Error("unknown command");
  await mount();
  expect(info?.disabled).toBe(false);
  expect(native.check).toHaveBeenCalledTimes(1);
});
