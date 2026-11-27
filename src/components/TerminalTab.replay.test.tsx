// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import type { TerminalSinks } from "../hosts/terminalTransport";

const mounted = vi.hoisted(() => ({ terminals: [] as XtermTerminal[], sinks: [] as TerminalSinks[] }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []), Channel: class {} }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null }) }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => ({ configs: [], available: [], status: {}, live: {}, agents: {}, epochs: {} }), useHostLive: () => null, useHostStatus: () => undefined }));
vi.mock("../hosts/terminalTransport", async original => ({
  ...await original<typeof import("../hosts/terminalTransport")>(),
  mountTerminal: (_tab: unknown, sinks: TerminalSinks) => {
    mounted.sinks.push(sinks);
    return { start: async () => {}, reattach: async () => {}, end() {} };
  },
}));
vi.mock("../lib/fonts", () => ({ detectMonoFontFamily: () => "monospace", ensureMonoFontsLoaded: async () => {} }));
vi.mock("@xterm/xterm", async original => {
  const actual = await original<typeof import("@xterm/xterm")>();
  return { ...actual, Terminal: class extends actual.Terminal {
    constructor(options: ConstructorParameters<typeof actual.Terminal>[0]) { super(options); mounted.terminals.push(this); }
    // Exercise the installed xterm write queue/parser/buffer without DOM rendering/GPU.
    open() {}
  } };
});
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class { activate() {} dispose() {} fit() {} } }));
vi.mock("@xterm/addon-web-links", () => ({ WebLinksAddon: class { activate() {} dispose() {} } }));

import { TerminalTab } from "./TerminalTab";

let root: Root | undefined, container: HTMLDivElement;
beforeEach(() => {
  mounted.terminals.length = 0; mounted.sinks.length = 0;
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("IntersectionObserver", class { observe() {} disconnect() {} });
  vi.stubGlobal("requestAnimationFrame", vi.fn(() => 1));
});
afterEach(async () => {
  await act(async () => root?.unmount()); root = undefined; container?.remove();
  vi.unstubAllGlobals(); vi.restoreAllMocks();
});
async function mountView(id: string) {
  container = document.createElement("div"); document.body.append(container); root = createRoot(container);
  await act(async () => root!.render(<TerminalTab tab={{ id, type: "terminal", title: "Terminal", shellMode: "raw" }} isActive={false} gitLazyPolling gitChangesTree={false} rightPanel="closed" onRightPanelChange={vi.fn()} terminalBgColor="#141413" defaultFontSize={14} defaultShellId="bash" fullscreenRendering={false} forceSyncOutput={false} webglRendering={false} terminalFontWeight={400} eagerInit={false} theme="dark" projectEncodedName="" showTerminalHeaderStats={false} onBranchSwitch={vi.fn()} onSkipPermissionsChange={vi.fn()} />));
  return { term: mounted.terminals[0], sinks: mounted.sinks[0] };
}
const bytes = (text: string) => new TextEncoder().encode(text);
const drain = (term: XtermTerminal) => new Promise<void>(resolve => term.write("", resolve));
function text(term: XtermTerminal) {
  const buffer = term.buffer.active;
  return Array.from({ length: buffer.length }, (_, line) => buffer.getLine(line)?.translateToString(true) ?? "").filter(Boolean);
}

it.each(["", "\x1b[31", "\x1b]0;unfinished-title", "\x1bPunfinished-control"])("the actual TerminalTab sink queues a parser-reset boundary after old writes and before replay (%j)", async partial => {
  const { term, sinks } = await mountView("replay");
  // Both chunks are in the installed xterm's asynchronous queue when reset is requested.
  await act(async () => {
    sinks.data(bytes("stale-first\r\n"));
    sinks.data(bytes(`stale-second${partial}`));
    expect(text(term)).toEqual([]);
    sinks.reset!();
    sinks.data(bytes("replacement-only"));
    sinks.exit(0);
    await drain(term);
  });
  expect(text(term)).toEqual(["replacement-only", "[Session ended]"]);
  expect(term.buffer.active.getLine(0)?.getCell(0)?.getFgColorMode()).toBe(0);
});
