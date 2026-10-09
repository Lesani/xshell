import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn(async () => ({ get: async () => undefined, set: async () => {}, save: async () => {} })) }));

import { hostInvoke, hostInvokeLive, hostQuery, HostUnavailableError, isStale, _resetStale } from "./hostInvoke";
import { cache } from "./cache";

const H = "h_ab12cd34";

beforeEach(() => {
  invoke.mockReset();
  cache._reset();
  _resetStale();
});

describe("hostInvoke", () => {
  it("local passes through", async () => {
    invoke.mockResolvedValueOnce(["s"]);
    await expect(hostInvoke(undefined, "get_sessions", { encodedName: "x" })).resolves.toEqual(["s"]);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke.mock.calls[0]).toEqual(["get_sessions", { encodedName: "x" }]);
  });

  it("local without args calls invoke with the command only", async () => {
    invoke.mockResolvedValueOnce([]);
    await hostInvoke(undefined, "list_claude_projects");
    expect(invoke.mock.calls[0]).toEqual(["list_claude_projects"]);
  });

  it("local never touches the cache", async () => {
    invoke.mockResolvedValueOnce(["s"]);
    await hostInvoke(undefined, "get_sessions", { encodedName: "x" });
    expect(cache.getState().hosts).toEqual({});
  });

  it("remote wraps in host_call", async () => {
    invoke.mockResolvedValue([]);
    await hostInvoke(H, "get_sessions", { encodedName: "x" });
    expect(invoke.mock.calls[0]).toEqual(["host_call", { host: H, method: "get_sessions", params: { encodedName: "x" } }]);
    await hostInvoke(H, "list_claude_projects");
    expect(invoke.mock.calls[1]).toEqual(["host_call", { host: H, method: "list_claude_projects", params: {} }]);
  });

  it("remote error string parity", async () => {
    invoke.mockRejectedValueOnce({ code: "remote", message: "boom" });
    const err = await hostInvoke(H, "git_checkout", { cwd: "/p", branch: "b" }).catch(e => e);
    expect(err).toBe("boom");
  });

  it("offline maps to HostUnavailableError", async () => {
    invoke.mockRejectedValueOnce({ code: "offline", message: "not connected" });
    const err = await hostInvoke<never>(H, "git_stage", { cwd: "/p", paths: [] }).catch((e: HostUnavailableError) => e);
    expect(err).toBeInstanceOf(HostUnavailableError);
    expect(err.code).toBe("offline");
    expect(err.host).toBe(H);
  });

  it("cacheable success then offline returns cache", async () => {
    invoke.mockResolvedValueOnce([{ id: "a" }]);
    await hostInvoke(H, "get_sessions", { encodedName: "x" });
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    await expect(hostInvoke(H, "get_sessions", { encodedName: "x" })).resolves.toEqual([{ id: "a" }]);
    invoke.mockRejectedValueOnce({ code: "incompatible", message: "old" });
    await expect(hostInvoke(H, "get_sessions", { encodedName: "x" })).resolves.toEqual([{ id: "a" }]);
  });

  it("offline without cache throws", async () => {
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    await expect(hostInvoke(H, "get_sessions", { encodedName: "x" })).rejects.toBeInstanceOf(HostUnavailableError);
  });

  it("non-cacheable offline throws even if a previous value exists", async () => {
    invoke.mockResolvedValueOnce({ is_repo: true });
    await hostInvoke(H, "get_git_status", { cwd: "/p" });
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    await expect(hostInvoke(H, "get_git_status", { cwd: "/p" })).rejects.toBeInstanceOf(HostUnavailableError);
  });

  it("timeouts do not fall back to the cache", async () => {
    invoke.mockResolvedValueOnce([1]);
    await hostInvoke(H, "get_sessions", { encodedName: "x" });
    invoke.mockRejectedValueOnce({ code: "timeout", message: "slow" });
    await expect(hostInvoke(H, "get_sessions", { encodedName: "x" })).rejects.toBeInstanceOf(HostUnavailableError);
  });
});

describe("per-result provenance (amendment 24)", () => {
  it("reconnect before refetch still shows stale; live fetch clears it", async () => {
    invoke.mockResolvedValueOnce([1]);
    expect(await hostQuery(H, "get_sessions", { encodedName: "x" })).toMatchObject({ value: [1], stale: false });
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    const cached = await hostQuery(H, "get_sessions", { encodedName: "x" });
    expect(cached).toMatchObject({ value: [1], stale: true });
    expect(isStale(H, "get_sessions", { encodedName: "x" })).toBe(true);
    // The Host reconnects: nothing has been re-fetched yet, so the value is still stale.
    expect(isStale(H, "get_sessions", { encodedName: "x" })).toBe(true);
    invoke.mockResolvedValueOnce([1, 2]);
    expect(await hostQuery(H, "get_sessions", { encodedName: "x" })).toMatchObject({ value: [1, 2], stale: false });
    expect(isStale(H, "get_sessions", { encodedName: "x" })).toBe(false);
  });

  it("live-only calls never use the cache", async () => {
    invoke.mockResolvedValueOnce({ present: true });
    await hostInvoke(H, "get_codex_usage");
    invoke.mockRejectedValueOnce({ code: "offline", message: "down" });
    await expect(hostInvokeLive(H, "get_codex_usage")).rejects.toBeInstanceOf(HostUnavailableError);
  });

  it("local results are never stale", async () => {
    invoke.mockResolvedValueOnce([]);
    expect((await hostQuery(undefined, "get_sessions", { encodedName: "x" })).stale).toBe(false);
    expect(isStale(undefined, "get_sessions", { encodedName: "x" })).toBe(false);
  });
});
