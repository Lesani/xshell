// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocalDaemonSettings } from "./LocalDaemonSettings";
import { C } from "../ring/copy";

const native = vi.hoisted(() => ({ invoke: vi.fn(), info: {} as Record<string, unknown>, keep: "ask", failSet: false }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => ({ status: {}, live: {} }), statusLabel: () => "" }));

let container: HTMLDivElement;
let root: Root;
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const toggle = () => container.querySelector<HTMLInputElement>(`input[aria-label="${C["quit.setting"]}"]`);

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  native.keep = "ask";
  native.failSet = false;
  native.info = { mode: "daemon", reason: null, persistent: { supported: true, enabled: true, running: "gui-bound", log: null }, daemon: { state: "ready", error: null, errorKind: null, log: null }, service: { mechanism: "systemd", linger: true, error: null } };
  native.invoke.mockReset();
  native.invoke.mockImplementation(async (command: string, args: Record<string, unknown>) => {
    if (command === "local_host_info") return native.info;
    if (command === "local_keep_sessions_get") return native.keep;
    if (command === "local_keep_sessions_set") { if (native.failSet) throw "settings are unavailable"; native.keep = args.value as string; return null; }
    throw new Error(`unexpected ${command}`);
  });
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});

describe("This computer: keep terminals", () => {
  it("no Persistent toggle or mode wording even if persistent.supported", async () => {
    await act(async () => { root.render(<LocalDaemonSettings />); });
    expect(container.textContent).not.toContain("Keep terminals running after quit");
    expect(container.textContent).not.toContain("ends with xshell");
    expect(container.querySelectorAll("input[type=checkbox]")).toHaveLength(1);
  });

  it("keep toggle reads get and writes {value:'always'}; a failed write reverts", async () => {
    await act(async () => { root.render(<LocalDaemonSettings />); });
    expect(container.textContent).toContain(C["quit.setting.body"]);
    expect(toggle()!.checked).toBe(false);
    await act(async () => { toggle()!.click(); });
    expect(native.invoke).toHaveBeenCalledWith("local_keep_sessions_set", { value: "always" });
    expect(toggle()!.checked).toBe(true);
    native.failSet = true;
    await act(async () => { toggle()!.click(); });
    expect(toggle()!.checked).toBe(true);
    expect(container.textContent).toContain(C["quit.setting.failed"]);
  });

  it("not offered without a kept-running Daemon (Windows: service null)", async () => {
    native.info = { ...native.info, service: null };
    await act(async () => { root.render(<LocalDaemonSettings />); });
    expect(q("[data-keep-sessions]")).toBeNull();
  });
});
