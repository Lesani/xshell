// @vitest-environment happy-dom
import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocalFailedView } from "./LocalFailedView";
import { C } from "../ring/copy";
import type { LocalStatus } from "../local/localInfo";

let container: HTMLDivElement;
let root: Root;
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const text = () => container.textContent ?? "";
const failed = (over: Partial<LocalStatus> = {}): LocalStatus => ({ mode: "daemon", reason: null, daemon: { state: "failed", error: "exited", errorKind: null, log: "/home/u/.xshell/log/xshelld.log" }, service: null, ...over });

let setInfo: (i: LocalStatus | null) => void = () => {};
function Harness({ initial, retry }: { initial: LocalStatus | null; retry: () => Promise<unknown> }) {
  const [info, set] = useState(initial);
  setInfo = set;
  return <LocalFailedView info={info} retry={retry} />;
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});

describe("Daemon-failed view", () => {
  it("mode failed shows the title and the full log path (default place without a Daemon)", async () => {
    await act(async () => { root.render(<Harness initial={{ mode: "failed", reason: "home unknown", daemon: null, service: null }} retry={async () => null} />); });
    expect(q("[role=alertdialog]")).not.toBeNull();
    expect(q("#local-failed-title")?.textContent).toBe(C["local.failed.title"]);
    expect(q("[data-local-log]")?.textContent).toBe("~/.xshell/log/xshelld.log");
    expect(text()).toContain("xshell couldn't start the background process for terminals. Check the log at ~/.xshell/log/xshelld.log, then try again.");
  });

  it("a failed Daemon names its own log", async () => {
    await act(async () => { root.render(<Harness initial={failed()} retry={async () => null} />); });
    expect(q("[data-local-log]")?.textContent).toBe("/home/u/.xshell/log/xshelld.log");
  });

  it("Retry calls retry, disabled while pending", async () => {
    let done!: () => void;
    const retry = vi.fn(() => new Promise<void>(r => { done = r; }));
    await act(async () => { root.render(<Harness initial={failed()} retry={retry} />); });
    await act(async () => { q("[data-local-retry]")!.click(); });
    expect(retry).toHaveBeenCalledOnce();
    expect((q("[data-local-retry]") as HTMLButtonElement).disabled).toBe(true);
    await act(async () => { q("[data-local-retry]")!.click(); });
    expect(retry).toHaveBeenCalledOnce();
    await act(async () => { done(); });
    expect((q("[data-local-retry]") as HTMLButtonElement).disabled).toBe(false);
  });

  it("local:daemon ready hides it", async () => {
    await act(async () => { root.render(<Harness initial={failed()} retry={async () => null} />); });
    await act(async () => { setInfo({ ...failed(), daemon: { state: "ready", error: null, errorKind: null, log: null } }); });
    expect(q("[role=alertdialog]")).toBeNull();
  });

  it("errorKind older-desktop shows Close the other xshell window first", async () => {
    await act(async () => { root.render(<Harness initial={failed({ daemon: { state: "failed", error: "older-desktop: x", errorKind: "older-desktop", log: null } })} retry={async () => null} />); });
    expect(q("#local-failed-title")?.textContent).toBe(C["local.older.title"]);
    expect(text()).toContain(C["local.older.body"]);
    expect(text()).not.toContain("older-desktop");
  });

  it("mode in-process or a starting Daemon renders nothing", async () => {
    await act(async () => { root.render(<Harness initial={{ mode: "in-process", reason: "test", daemon: null, service: null }} retry={async () => null} />); });
    expect(container.innerHTML).toBe("");
    await act(async () => { setInfo({ ...failed(), daemon: { state: "starting", error: null, errorKind: null, log: null } }); });
    expect(container.innerHTML).toBe("");
  });
});
