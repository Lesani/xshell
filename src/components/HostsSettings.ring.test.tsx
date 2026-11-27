// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterAll, afterEach, beforeAll, beforeEach, expect, it, vi } from "vitest";
import { HostsSettings } from "./HostsSettings";
import { registry } from "../hosts/registry";
import { addToRingErrorCode } from "../hosts/sshRing";
import type { AvailableHost, AvailableSnapshot, HostConfig, HostStatus } from "../hosts/types";
import type { RingStatus } from "../ring/types";

// Add to Ring on a saved SSH Host's row (#115).

const native = vi.hoisted(() => ({ invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({ get: async () => null, set: async () => {}, save: async () => {} }) }));

const HOST: HostConfig = { id: "h_abcdefgh", name: "dev box", sshTarget: "dev" };

function row(member: boolean): AvailableHost {
  return { id: HOST.id, aliases: [HOST.id], name: HOST.name, color: null, daemonKey: member ? "key" : null, routes: member ? ["ssh", "relay"] : ["ssh"], activeRoute: "ssh", reachability: { kind: "online" } };
}

function hostStatus(patch: Partial<HostStatus> = {}): HostStatus {
  return { host: HOST.id, status: "connected", phase: null, lastError: null, errorHint: null, daemonVersion: "1.6.0", desktopVersion: "1.6.0", protocol: 1, os: "linux", arch: "x86_64", incompatibleReason: null, nextRetryAt: null, sinceMs: 0, daemonCapabilities: ["ring"], ...patch };
}

function ringStatus(patch: Partial<RingStatus> = {}): RingStatus {
  return { enabled: true, ringId: "ring", version: 3, relayUrl: "ws://127.0.0.1:1", hostedRelayUrl: "wss://relay", connection: "connected", retryIn: null, connectionError: null, limited: false, problem: null, problemDetail: null, members: [], quotaResetAt: null, relayHosted: false, local: "daemon", hosts: [], membership: "member", removed: null, oldRing: null, finishingOffline: false, busy: false, ...patch };
}

let root: Root;
let container: HTMLDivElement;
let sequence = 1;
let mocks: { ring?: RingStatus | null; add?: () => Promise<unknown> } = {};

native.invoke.mockImplementation(async (command: string) => {
  if (command === "hosts_available") return { sequence: 1, hosts: [row(false)] } satisfies AvailableSnapshot;
  if (command === "hosts_status") return [{ status: hostStatus(), terminals: null }];
  if (command === "local_host_info") return { mode: "in-process", reason: null };
  if (command === "host_call") return { installed: false };
  if (command === "ring_status") { if (mocks.ring === null) throw "mobile access is unavailable"; return mocks.ring ?? ringStatus(); }
  if (command === "ring_join_overview") return { owner: true, joins: [] };
  if (command === "ring_add_ssh_host") return (mocks.add ?? (async () => ({ names: ["dev box"], version: 4 })))();
  return null;
});

// The registry runs once per file; each test sets the Host's row and status on it.
beforeAll(async () => { await registry.init([HOST]); });
afterAll(() => { registry._dispose(); });

async function render(opts: { member?: boolean; status?: Partial<HostStatus>; ring?: RingStatus | null; add?: () => Promise<unknown> } = {}) {
  mocks = { ring: opts.ring, add: opts.add };
  registry.applyAvailable({ sequence: ++sequence, hosts: [row(!!opts.member)] });
  registry.applyStatus(hostStatus(opts.status));
  await act(async () => { root.render(<HostsSettings onSave={vi.fn()} onRemove={vi.fn()} />); });
}

const q = (selector: string) => container.querySelector(selector);
const addButton = () => q("[data-ring-add]") as HTMLButtonElement | null;

async function fresh() {
  await act(async () => { root.unmount(); });
  root = createRoot(container);
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

it("offers Add to Ring on a connected Host that is not a member", async () => {
  await render();
  expect(addButton()?.textContent).toContain("Add to Ring");
  expect(addButton()?.disabled).toBe(false);
  expect(q("[data-ring-member]")).toBeNull();
});

it("shows the member label instead of the button for a Ring member", async () => {
  await render({ member: true });
  expect(q("[data-ring-member]")?.textContent).toContain("In your Ring");
  expect(addButton()).toBeNull();
});

it("disables the button with the reason while the Host is offline", async () => {
  await render({ status: { status: "offline", daemonCapabilities: [] } });
  expect(addButton()?.disabled).toBe(true);
  expect(addButton()?.title).toBe("Connect to dev box before adding it to your Ring.");
  expect(q("[data-ring-offline-hint]")?.textContent).toBe("Connect to dev box before adding it to your Ring.");
});

it("shows nothing while this Desktop is in no Ring, or for a Windows Host", async () => {
  await render({ ring: ringStatus({ enabled: false, membership: "none", ringId: null }) });
  expect(q("[data-ring-action]")).toBeNull();
  await fresh();
  await render({ status: { os: "windows" } });
  expect(q("[data-ring-action]")).toBeNull();
  await fresh();
  await render({ ring: null });
  expect(q("[data-ring-action]")).toBeNull();
});

it("reads Adding… while it runs, then the success line until the row is a member", async () => {
  let finish: (v: unknown) => void = () => {};
  await render({ add: () => new Promise(resolve => { finish = resolve; }) });
  await act(async () => { addButton()!.click(); });
  expect(addButton()?.textContent).toContain("Adding…");
  expect(addButton()?.disabled).toBe(true);
  expect(native.invoke).toHaveBeenCalledWith("ring_add_ssh_host", { host: HOST.id });
  await act(async () => { finish({ names: ["dev box"], version: 4 }); });
  expect(q("[data-ring-added]")?.textContent).toBe("dev box is now in your Ring.");
  expect(addButton()?.textContent).toContain("Add to Ring");
  // The Host's row becomes a member: the label replaces the button and the line.
  await act(async () => { registry.applyAvailable({ sequence: ++sequence, hosts: [row(true)] }); });
  expect(q("[data-ring-member]")).not.toBeNull();
  expect(q("[data-ring-added]")).toBeNull();
  expect(addButton()).toBeNull();
});

const SENTENCES: [string, string][] = [
  ["not-ready", "xshell on this computer can’t add machines right now; try again in a moment."],
  ["offline", "dev box is not connected; wait for it to connect, then try again."],
  ["unsupported", "Can’t add dev box this way; update xshell on that machine first."],
  ["windows", "dev box runs Windows; add it to your Ring from xshell on that machine."],
  ["own-desktop", "dev box runs its own xshell app; add it to your Ring from xshell on that machine."],
  ["other-ring", "dev box is already in another Ring; remove it from that Ring before adding it to yours."],
  ["rate-limited", "Too many attempts; wait a few minutes, then try again."],
  ["unfinished", "An earlier attempt on dev box is still finishing; try again in a minute."],
  ["expired", "Adding dev box took too long; try again."],
  ["service", "dev box couldn’t reach the service your Ring uses; check that machine’s internet connection, then try again."],
  ["busy", "Another machine is being added; wait until it is done, then try again."],
  ["failed", "Couldn’t add dev box to your Ring; try again."],
];

it.each(SENTENCES)("shows the approved sentence for %s, and Retry is the same button", async (code, sentence) => {
  await render({ add: async () => { throw `${code}: some detail for the log`; } });
  await act(async () => { addButton()!.click(); });
  const line = q("[data-ring-error]");
  expect(line?.getAttribute("data-ring-error")).toBe(code);
  expect(line?.textContent).toBe(sentence);
  expect(container.textContent).not.toContain("some detail for the log");
  expect(addButton()?.disabled).toBe(false);
  expect(addButton()?.textContent).toContain("Add to Ring");
});

it("reads an unknown error as failed", () => {
  expect(addToRingErrorCode("other: something")).toBe("failed");
  expect(addToRingErrorCode(new Error("boom"))).toBe("failed");
  expect(addToRingErrorCode("service: x")).toBe("service");
});
