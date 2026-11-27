// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { RegistrySnapshot } from "../hosts/registry";
import type { AvailableHost, HostStatus } from "../hosts/types";

// "Skip this version" in the update bar (Lesani/xshell#117). settings.json is one map that
// outlives the modules: `mount()` loads the bar and its stores anew, as a Desktop restart does.
const disk = vi.hoisted(() => ({ settings: new Map<string, unknown>() }));
const snap = vi.hoisted(() => ({ current: null as unknown as RegistrySnapshot }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({
  get: async (key: string) => disk.settings.get(key) ?? null,
  set: async (key: string, value: unknown) => { disk.settings.set(key, JSON.parse(JSON.stringify(value))); },
}) }));
vi.mock("../hosts/useHosts", async original => ({ ...await original<typeof import("../hosts/useHosts")>(), useHostsSnapshot: () => snap.current }));

const row = (id: string, name: string): AvailableHost => ({ id, aliases: [id], name, color: null, daemonKey: null, routes: ["relay"], activeRoute: "relay", reachability: { kind: "online" } });
const A = row("r_a", "box-a"), B = row("r_b", "box-b");
const build = { commit: "85ee920a1b2c", dirty: null, time: 7 };
const release = (host: string, version = "1.6.0"): HostStatus => ({ host, status: "upgrade-pending", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.6.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 1, linkGeneration: 1, updateAction: "update", updateSource: "release", updateVersion: version, desktopBuild: build });
const desktop = (host: string): HostStatus => ({ ...release(host), updateSource: "desktop", updateVersion: "1.6.0" });
const replace = (host: string): HostStatus => ({ ...desktop(host), status: "build-differs", daemonVersion: "1.6.0", updateAction: "replace" });

type Bar = { show: (status: Record<string, HostStatus>) => Promise<void>; html: () => string; rows: () => string[]; skip: (name: string) => Promise<void>; unmount: () => void };
let mounted: Bar[] = [];
async function mount(status: Record<string, HostStatus>): Promise<Bar> {
  vi.resetModules();
  const { act, createElement } = await import("react");
  const { createRoot } = await import("react-dom/client");
  const { HostUpdateBanner } = await import("./HostUpdateBanner");
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const rowsOf = () => [...container.querySelectorAll(".host-update-action")];
  const bar: Bar = {
    show: async next => {
      snap.current = { configs: [], available: [A, B], status: next, live: {}, agents: {}, epochs: {} } as unknown as RegistrySnapshot;
      // The stored targets load after the first paint; settle that too.
      await act(async () => { root.render(createElement(HostUpdateBanner, { host: undefined })); await new Promise(r => setTimeout(r, 0)); });
    },
    html: () => container.innerHTML,
    rows: () => rowsOf().map(r => r.textContent ?? ""),
    skip: async name => {
      const button = rowsOf().find(r => r.textContent?.includes(name))!.querySelector<HTMLButtonElement>("[data-update-skip]")!;
      await act(async () => { button.click(); await new Promise(r => setTimeout(r, 0)); });
    },
    unmount: () => { act(() => root.unmount()); container.remove(); },
  };
  mounted.push(bar);
  await bar.show(status);
  return bar;
}

beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); disk.settings.clear(); });
afterEach(() => { for (const bar of mounted) bar.unmount(); mounted = []; vi.unstubAllGlobals(); });

describe("Skip this version", () => {
  it("is offered beside the action button in the bar, not with the Settings usage", async () => {
    const bar = await mount({ r_a: release("r_a"), r_b: replace("r_b") });
    expect(bar.rows()).toHaveLength(2);
    for (const text of bar.rows()) expect(text).toContain("Skip this version");
    expect(bar.rows()[0]).toMatch(/UpdateSkip this version$/);
    expect(bar.rows()[1]).toMatch(/Use this Desktop’s buildSkip this version$/);
    const button = document.querySelector<HTMLButtonElement>("[data-update-skip]")!;
    expect(button.className).toBe("btn btn-ghost settings-action-btn");
    expect(button.disabled).toBe(false);

    // Settings → Hosts and the Ring rows mount the same action without `skippable`.
    const { createElement } = await import("react");
    const { renderToStaticMarkup } = await import("react-dom/server");
    const { HostUpdateAction } = await import("./HostUpdateAction");
    const settings = renderToStaticMarkup(createElement(HostUpdateAction, { host: { id: "r_a", name: "box-a" }, status: release("r_a"), description: true }));
    expect(settings).toContain(">Update</button>");
    expect(settings).not.toContain("data-update-skip");
    expect(settings).not.toContain("Skip this version");
  });

  it("is absent while an update runs and on a row that only explains", async () => {
    const bar = await mount({ r_a: { ...release("r_a"), phase: "preparing" }, r_b: { ...replace("r_b"), updateUnavailable: "desktop-owned" } });
    expect(bar.rows()).toHaveLength(2);
    expect(bar.html()).not.toContain("data-update-skip");
  });

  it("is disabled with the Update button while a confirmation is open", async () => {
    const bar = await mount({ r_a: release("r_a") });
    const { act } = await import("react");
    const { hostUpdates } = await import("../hosts/hostUpdate");
    act(() => hostUpdates.ask({ id: "r_a", name: "box-a" }, release("r_a")));
    const buttons = [...document.querySelectorAll<HTMLButtonElement>(".host-update-action button")];
    expect(buttons.map(b => b.disabled)).toEqual([true, true]);
    act(() => hostUpdates.cancel());
    expect(bar.rows()).toHaveLength(1);
  });

  it("hides that machine's row only, until another target is offered", async () => {
    const bar = await mount({ r_a: release("r_a"), r_b: release("r_b") });
    await bar.skip("box-a");
    expect(bar.rows()).toHaveLength(1);
    expect(bar.rows()[0]).toContain("box-b");
    // An earlier attempt's error is hidden with the skipped offer, also once the machine is offline.
    await bar.show({ r_a: { ...release("r_a"), lastError: "stage failed", errorHint: "update-failed" }, r_b: release("r_b") });
    expect(bar.html()).not.toContain("box-a");
    await bar.show({ r_a: { ...release("r_a"), status: "offline", updateAction: null, updateSource: null, updateVersion: null, lastError: "offline: deadline reached", errorHint: "update-failed" }, r_b: release("r_b") });
    expect(bar.html()).not.toContain("box-a");
    // A newer release is another target.
    await bar.show({ r_a: release("r_a", "1.7.0"), r_b: release("r_b") });
    expect(bar.rows().map(t => t.slice(0, 5))).toEqual(["box-a", "box-b"]);
    expect(bar.rows()[0]).toContain("The latest release is 1.7.0.");
  });

  it("tells this Desktop's build from a release, and a replace from an update of the same version", async () => {
    const bar = await mount({ r_a: desktop("r_a") });
    await bar.skip("box-a");
    expect(bar.html()).toBe("");
    // The same number from a release, and this Desktop's build replacing that version.
    await bar.show({ r_a: release("r_a") });
    expect(bar.rows()).toHaveLength(1);
    await bar.show({ r_a: desktop("r_a") });
    expect(bar.html()).toBe("");
    await bar.show({ r_a: replace("r_a") });
    expect(bar.rows()[0]).toContain("Use this Desktop’s build");
    await bar.skip("box-a");
    expect(bar.html()).toBe("");
    // Another build of this Desktop's version is another target.
    await bar.show({ r_a: { ...replace("r_a"), desktopBuild: { ...build, commit: "0123456789ab" } } });
    expect(bar.rows()).toHaveLength(1);
  });

  it("survives a Desktop restart", async () => {
    const first = await mount({ r_a: release("r_a"), r_b: release("r_b") });
    await first.skip("box-a");
    expect(disk.settings.get("update_skipped_versions")).toEqual({ r_a: "release:1.6.0" });
    first.unmount(); mounted = [];
    const second = await mount({ r_a: release("r_a"), r_b: release("r_b") });
    expect(second.rows()).toHaveLength(1);
    expect(second.rows()[0]).toContain("box-b");
    await second.show({ r_a: release("r_a", "1.7.0"), r_b: release("r_b") });
    expect(second.rows()).toHaveLength(2);
  });

  it("shows the progress of a skipped machine's running update", async () => {
    const bar = await mount({ r_a: release("r_a") });
    await bar.skip("box-a");
    expect(bar.html()).toBe("");
    await bar.show({ r_a: { ...release("r_a"), phase: "preparing" } });
    expect(bar.rows()).toEqual(["box-a: Downloading update…"]);
    await bar.show({ r_a: { ...release("r_a"), phase: "upgrading", updateAction: null, updateSource: null, updateVersion: null } });
    expect(bar.rows()).toEqual(["box-a: Restarting…"]);
    // It failed: the skipped offer and its error are hidden again.
    await bar.show({ r_a: { ...release("r_a"), lastError: "stage failed", errorHint: "update-failed" } });
    expect(bar.html()).toBe("");
  });

  it("renders nothing when every row is skipped", async () => {
    const bar = await mount({ r_a: release("r_a"), r_b: replace("r_b") });
    await bar.skip("box-a");
    await bar.skip("box-b");
    expect(bar.html()).toBe("");
  });
});
