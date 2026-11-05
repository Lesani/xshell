import { describe, expect, it, vi } from "vitest";
import { CustomTitleController, customTitleError, customTitleUnavailable, captureCustomTitleTarget, customTitleTargetMatches } from "./customTitle";
import type { RegistrySnapshot } from "../hosts/registry";
import type { HostStatus, TerminalInfo } from "../hosts/types";
import type { Tab } from "../types";
import { fmt } from "../hosts/strings";

const terminal: TerminalInfo = { terminal: "uuid", spec: { cwd: "/p" }, meta: { title: "Automatic" }, createdAtMs: 0, pid: 1, exitCode: null };
const status: HostStatus = { host: "local", status: "connected", phase: null, lastError: null, errorHint: null, daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 0, configGeneration: 2, daemonCapabilities: ["term.custom-title"] };
function fixture() {
  let tabs: Tab[] = [{ id: "tab", terminal: "uuid", type: "terminal", title: "Automatic", customTitle: "Original" }];
  const snap: RegistrySnapshot = { configs: [], status: { local: { ...status } }, live: { local: [terminal] }, agents: {}, epochs: {} };
  const send = vi.fn<(_: string, args: Record<string, unknown>) => Promise<unknown>>().mockResolvedValue(undefined);
  const controller = new CustomTitleController(() => tabs, () => snap, send);
  return { controller, snap, send, get tabs() { return tabs; }, set tabs(value: Tab[]) { tabs = value; } };
}
const deferred = () => { let resolve!: () => void, reject!: (e: unknown) => void; const promise = new Promise<void>((ok, no) => { resolve = ok; reject = no; }); return { promise, resolve, reject }; };

describe("CustomTitleController", () => {
  it("local_wire_host_and_terminal_uuid_are_captured_without_automatic_edits", async () => {
    const f = fixture();
    expect(f.controller.open(f.tabs[0])).toBe(true);
    expect(f.controller.getSnapshot().dialog?.target).toMatchObject({ host: "local", terminal: "uuid", tabId: "tab", configGeneration: 2 });
    await f.controller.submit("Shared");
    expect(f.send).toHaveBeenCalledExactlyOnceWith("host_term_update", { host: "local", terminal: "uuid", meta: { customTitle: "Shared" } });
    expect(f.tabs[0]).toMatchObject({ title: "Automatic", customTitle: "Original" });
    expect(f.controller.getSnapshot().dialog).toBeNull();
  });
  it("captures_a_Remote_Host_and_sends_explicit_null_to_clear", async () => {
    const f = fixture();
    f.tabs = [{ ...f.tabs[0], host: "h_other" }];
    f.snap.configs = [{ id: "h_other", name: "Other", sshTarget: "other" }];
    f.snap.status.h_other = { ...status, host: "h_other" };
    f.snap.live.h_other = [terminal];
    f.controller.open(f.tabs[0]);
    await f.controller.submit(null);
    expect(f.send).toHaveBeenCalledExactlyOnceWith("host_term_update", { host: "h_other", terminal: "uuid", meta: { customTitle: null } });
  });
  it("incapable_offline_or_unlisted_target_is_refused", async () => {
    const f = fixture();
    f.snap.status.local.daemonCapabilities = [];
    expect(customTitleUnavailable(f.tabs[0], f.snap)).toBe(fmt("tab.rename.unsupported"));
    expect(f.controller.open(f.tabs[0])).toBe(false);
    f.snap.status.local = { ...status, status: "offline" };
    expect(customTitleUnavailable(f.tabs[0], f.snap)).toBe(fmt("tab.rename.offline"));
    expect(f.controller.open(f.tabs[0])).toBe(false);
    expect(customTitleUnavailable({ ...f.tabs[0], terminal: undefined }, f.snap)).toBeNull();
    expect(customTitleUnavailable({ ...f.tabs[0], terminal: undefined, host: "remote" }, f.snap)).toBe(fmt("tab.rename.gone"));
    f.snap.status.local = { ...status };
    f.snap.live.local = [];
    expect(f.controller.open(f.tabs[0])).toBe(false);
    expect(f.send).not.toHaveBeenCalled();
  });
  it("rechecks_usability_and_capability_immediately_before_send", async () => {
    const f = fixture(); f.controller.open(f.tabs[0]);
    f.snap.status.local.status = "offline";
    await f.controller.submit("No");
    expect(f.controller.getSnapshot().dialog?.error).toBe(fmt("tab.rename.offline"));
    f.snap.status.local = { ...status, daemonCapabilities: [] };
    await f.controller.submit("No");
    expect(f.controller.getSnapshot().dialog?.error).toBe(fmt("tab.rename.unsupported"));
    expect(f.send).not.toHaveBeenCalled();
  });
  it.each(["remote", "indeterminate", "timeout"])("failed_save_never_installs_override_and_retry_is_explicit (%s)", async code => {
    const f = fixture(); f.send.mockRejectedValueOnce({ code, message: "disk or response lost" });
    f.controller.open(f.tabs[0]); await f.controller.submit("Draft");
    expect(f.controller.getSnapshot().dialog?.error).toBe(code === "remote" ? fmt("tab.rename.failed", { error: "disk or response lost" }) : fmt("tab.rename.notConfirmed"));
    expect(f.tabs[0].customTitle).toBe("Original");
    f.snap.status.local = { ...status, status: "offline" }; f.controller.invalidate();
    f.snap.status.local = { ...status }; f.controller.invalidate();
    expect(f.send).toHaveBeenCalledTimes(1);
    await f.controller.submit("Draft");
    expect(f.send).toHaveBeenCalledTimes(2);
    expect(f.controller.getSnapshot().dialog).toBeNull();
  });
  it("echo_before_indeterminate_response_is_not_saved_success", async () => {
    const f = fixture(), d = deferred(); f.send.mockReturnValue(d.promise);
    f.controller.open(f.tabs[0]); const request = f.controller.submit("Draft");
    f.tabs = [{ ...f.tabs[0], customTitle: "Draft" }];
    d.reject({ code: "indeterminate", message: "sync" }); await request;
    expect(f.controller.getSnapshot().dialog?.error).toBe(fmt("tab.rename.notConfirmed"));
  });
  it("newer_broadcast_wins_over_old_success_reply", async () => {
    const f = fixture(), d = deferred(); f.send.mockReturnValue(d.promise);
    f.controller.open(f.tabs[0]); const request = f.controller.submit("Older");
    f.tabs = [{ ...f.tabs[0], customTitle: "Newer writer" }];
    d.resolve(); await request;
    expect(f.tabs[0].customTitle).toBe("Newer writer");
  });
  it.each(["closed", "generation", "configuration", "dismissed"])("late_result_does_not_revive_or_touch_another_target (%s)", async change => {
    const f = fixture(), d = deferred(); f.send.mockReturnValue(d.promise);
    f.controller.open(f.tabs[0]); const request = f.controller.submit("Sent");
    if (change === "closed") f.tabs = [];
    if (change === "generation") f.snap.status.local.configGeneration = 3;
    if (change === "configuration") f.snap.configs = [{ id: "local", name: "Replaced", sshTarget: "elsewhere" }];
    if (change === "dismissed") f.controller.dismiss(); else f.controller.invalidate();
    expect(f.controller.getSnapshot().dialog).toBeNull();
    f.tabs = [{ id: "other", terminal: "other-uuid", type: "terminal", title: "Other" }];
    expect(f.controller.open(f.tabs[0])).toBe(false); // cannot duplicate an outstanding request
    d.reject({ code: "remote", message: "late" }); await request;
    expect(f.controller.getSnapshot()).toEqual({ dialog: null, pending: false });
    expect(f.tabs[0].title).toBe("Other");
  });
  it("captured_menu_target_cannot_be_rebound_and_pre_send_invalidation_sends_nothing", async () => {
    const f = fixture(); const target = captureCustomTitleTarget(f.tabs[0], f.snap);
    f.snap.status.local.configGeneration = 3;
    expect(f.controller.open(f.tabs[0], target)).toBe(false);
    expect(f.controller.open(f.tabs[0])).toBe(true);
    f.snap.status.local.configGeneration = 4;
    await f.controller.submit("Must not send");
    expect(f.send).not.toHaveBeenCalled(); expect(f.controller.getSnapshot().dialog).toBeNull();
  });
  it("duplicate_submit_is_disabled_and_invalid_drafts_never_send", async () => {
    const f = fixture(), d = deferred(); f.send.mockReturnValue(d.promise);
    f.controller.open(f.tabs[0]); await f.controller.submit("\nBad"); expect(f.send).not.toHaveBeenCalled();
    const request = f.controller.submit("One"); await f.controller.submit(null);
    expect(f.controller.getSnapshot().dialog?.operation).toBe("save");
    expect(f.send).toHaveBeenCalledTimes(1); d.resolve(); await request;
  });
  it("in_process_route_captures_launch_identity_not_mutable_automatic_metadata", async () => {
    let tabs: Tab[] = [{ id: "local", type: "terminal", title: "Auto", projectPath: "/p", createdAt: 7, agent: "pi" }];
    const f = fixture(), save = vi.fn(async () => {});
    const controller = new CustomTitleController(() => tabs, () => f.snap, f.send, save);
    const target = captureCustomTitleTarget(tabs[0], f.snap)!;
    expect(target).toMatchObject({ kind: "in-process", tabId: "local", createdAt: 7 });
    controller.open(tabs[0], target);
    tabs = [{ ...tabs[0], title: "New auto", sessionId: "discovered", lastActiveAt: 99, groupId: "g" }];
    f.snap.status.local.configGeneration = 77; controller.invalidate();
    expect(controller.getSnapshot().dialog).not.toBeNull();
    await controller.submit("\u2003e\u0301 📝\ufeff\u2003");
    expect(save).toHaveBeenCalledExactlyOnceWith(target, "e\u0301 📝\ufeff"); expect(f.send).not.toHaveBeenCalled();
    expect(tabs[0]).toMatchObject({ title: "New auto", sessionId: "discovered" });
    for (const change of [{ createdAt: 8 }, { agent: "codex" as const }, { shellMode: "raw" as const }, { shellId: "zsh" }, { projectPath: "/other" }, { terminal: "uuid" }, { host: "other" }]) {
      expect(customTitleTargetMatches(target, { ...tabs[0], ...change }, f.snap)).toBe(false);
    }
  });
  it.each(["closed", "replacement", "daemon"])("in_process_%s_before_submit_never_calls_local_or_daemon_writer", async change => {
    let tabs: Tab[] = [{ id: "local", type: "terminal", title: "Auto", createdAt: 7 }];
    const f = fixture(), save = vi.fn(); const c = new CustomTitleController(() => tabs, () => f.snap, f.send, save);
    c.open(tabs[0]);
    tabs = change === "closed" ? [] : [{ ...tabs[0], ...(change === "daemon" ? { terminal: "uuid" } : { createdAt: 8 }) }];
    await c.submit("No"); expect(save).not.toHaveBeenCalled(); expect(f.send).not.toHaveBeenCalled(); expect(c.getSnapshot().dialog).toBeNull();
  });
  it("in_process_invalid_drafts_never_save_double_submit_is_one_and_late_error_cannot_revive", async () => {
    const tab: Tab = { id: "local", type: "terminal", title: "Auto" }, f = fixture(), d = deferred();
    const save = vi.fn().mockReturnValueOnce(d.promise);
    const c = new CustomTitleController(() => [tab], () => f.snap, f.send, save); c.open(tab);
    for (const value of ["", "\u2003", "📝".repeat(257), "name\u2028", "\tname", "name\x9f"]) await c.submit(value);
    expect(save).not.toHaveBeenCalled();
    const first = c.submit("📝".repeat(256)); await c.submit("Second"); expect(save).toHaveBeenCalledTimes(1);
    c.dismiss(); d.reject(new Error("late failure")); await first;
    expect(c.getSnapshot()).toEqual({ dialog: null, pending: false }); expect(f.send).not.toHaveBeenCalled();
  });
  it("validation_counts_scalars_and_uses_protocol_whitespace_not_BOM", () => {
    expect(customTitleError("📝".repeat(256))).toBeNull();
    expect(customTitleError("📝".repeat(257))).toBe(fmt("tab.rename.tooLong"));
    expect(customTitleError("\u2003 \u0085")).toBe(fmt("tab.rename.controls"));
    expect(customTitleError("\u2003 ")).toBe(fmt("tab.rename.blank"));
    expect(customTitleError("\ufeff")).toBeNull();
    expect(customTitleError("\u2003 name \u2003")).toBeNull();
    for (const control of ["\n", "\r", "\t", "\x00", "\x7f", "\x9f", "\u2028", "\u2029"]) expect(customTitleError(`name${control}`)).toBe(fmt("tab.rename.controls"));
  });
});
