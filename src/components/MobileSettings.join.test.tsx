// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MobileSettings } from "./MobileSettings";
import { fmt, S } from "../ring/strings";
import type { JoinEvent, JoinOverview, MemberView, RingStatus } from "../ring/types";

const native = vi.hoisted(() => ({ invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>(), settings: new Map<string, unknown>(), gate: { get: null as Promise<void> | null, set: null as Promise<void> | null } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({
  get: async (k: string) => { if (native.gate.get) await native.gate.get; return native.settings.get(k) ?? null; },
  set: async (k: string, v: unknown) => { if (native.gate.set) await native.gate.set; native.settings.set(k, v); },
  save: async () => {},
}) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));

const WORDS = "acid-baker-cable-dance-eagle-fable-gamma-habit";
const FAR = Math.floor(Date.now() / 1000) + 600;
const me: MemberView = { name: "desk", role: "desktop", signKey: "d", thisApp: true, thisComputer: true, hostId: null, removable: false, presence: { kind: "online" }, enrollmentId: "u1", kind: "computer", daemonKey: "dk", daemonPresence: { kind: "online" } };

function status(over: Partial<RingStatus> = {}): RingStatus {
  return {
    enabled: false, ringId: null, version: null, relayUrl: null, hostedRelayUrl: "wss://r", connection: "off",
    retryIn: null, connectionError: null, limited: false, farewells: [], problem: null, problemDetail: null,
    members: [], quotaResetAt: null, relayHosted: true, local: "daemon", hosts: [], approval: "none", format: null, busy: false, ...over,
  };
}
const enrolled = (over: Partial<RingStatus> = {}) => status({ enabled: true, ringId: "abcdefgh", version: 2, relayUrl: "wss://r", connection: "connected", members: [me], approval: "ready", ...over });
const preview = { ringId: "abcdefgh1234", ringShort: "abcdefgh", approver: "tower", computer: "laptop", expiresAt: FAR };

let container: HTMLDivElement;
let root: Root;
let commands: Record<string, (args: Record<string, unknown>) => unknown>;
const calls = () => native.invoke.mock.calls.map(([c]) => c as string);
const argsOf = (command: string) => native.invoke.mock.calls.filter(([c]) => c === command).map(([, a]) => a as Record<string, unknown>);

function setup(s: RingStatus, overview: JoinOverview = { owner: true, joins: [] }, extra: typeof commands = {}) {
  commands = { ring_status: () => s, ring_join_overview: () => overview, ring_join_cancel: () => undefined, ...extra };
  native.invoke.mockImplementation(async (command: string, args: Record<string, unknown>) => {
    const f = commands[command];
    if (!f) throw new Error(`unexpected ${command}`);
    return f(args ?? {});
  });
}
async function mount(visible = true) {
  await act(async () => { root.render(<MobileSettings visible={visible} />); });
}
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)!({ payload }); }); };
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const click = async (sel: string) => { await act(async () => { q(sel)!.click(); }); };
const text = () => container.textContent ?? "";
const joinEvent = (operation: string, state: JoinEvent["state"], over: Partial<JoinEvent> = {}): JoinEvent => ({ operation, origin: "gui", state, expiresAt: FAR, ...over });

// Starts Join ring and returns its operation.
async function startJoin(): Promise<string> {
  commands.ring_join_start = (a) => ({ operation: a.operation, words: WORDS, expiresAt: FAR });
  await click("[data-ring-join-action]");
  return argsOf("ring_join_start")[0].operation as string;
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  native.invoke.mockReset();
  native.handlers.clear();
  native.settings.clear();
  native.gate.get = null;
  native.gate.set = null;
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});

describe("unpaired Desktop: Create ring / Join ring", () => {
  it("Create ring calls ring_create_computer, never ring_enable", async () => {
    setup(status(), undefined, { ring_create_computer: () => enrolled() });
    await mount();
    expect(text()).toContain(S["ring.description"]);
    expect(text()).toContain(S["ring.localHint"]);
    await click("[data-ring-create]");
    expect(calls()).toContain("ring_create_computer");
    expect(calls()).not.toContain("ring_enable");
    expect(text()).toContain(S["ring.created"]);
  });

  it("Create ring failure while the local Host registers shows Host not ready", async () => {
    setup(status(), undefined, { ring_create_computer: () => { throw "not_registered: this computer's Host is not registered yet"; } });
    await mount();
    await click("[data-ring-create]");
    expect(text()).toContain(S["ring.hostNotReady"]);
  });

  it("recovered settings keep Start over through ring_enable", async () => {
    setup(status({ problem: "recovered", problemDetail: "/x" }), undefined, { ring_enable: () => enrolled() });
    await mount();
    expect(q("[data-ring-create]")).toBeNull();
    await act(async () => { Array.from(container.querySelectorAll("button")).find(b => b.textContent?.includes(S["mobile.enable"]))!.click(); });
    expect(argsOf("ring_enable")).toEqual([{ startOver: true }]);
    expect(calls()).not.toContain("ring_create_computer");
  });

  it("Join shows the words 4+4, then consent; Confirm sends the operation", async () => {
    setup(status(), undefined, { ring_join_confirm: () => undefined });
    await mount();
    const op = await startJoin();
    const words = q("[data-ring-words]")!;
    expect(words.children[0].textContent).toBe("acid baker cable dance");
    expect(words.children[1].textContent).toBe("eagle fable gamma habit");
    expect(text()).toContain(S["ring.joinInstruction"]);
    expect(text()).toContain(S["ring.codeLifetime"]);
    await emit("ring:join", joinEvent(op, "awaitingConfirmation", { preview }));
    expect(q("[data-ring-words]")).toBeNull();
    expect(text()).toContain(S["ring.consentTitle"]);
    expect(text()).toContain("“tower” has requested to add this computer, “laptop”, to Ring abcdefgh.");
    await click("[data-ring-confirm]");
    expect(argsOf("ring_join_confirm")).toEqual([{ operation: op }]);
    await emit("ring:join", joinEvent(op, "completing"));
    expect(text()).toContain(S["ring.completing"]);
    expect(q("[data-ring-new-code]")).toBeNull();
  });

  it("Decline ends the code and offers Generate new code", async () => {
    setup(status(), undefined, { ring_join_decline: () => undefined });
    await mount();
    const op = await startJoin();
    await emit("ring:join", joinEvent(op, "awaitingConfirmation", { preview }));
    await click("[data-ring-decline]");
    expect(argsOf("ring_join_decline")).toEqual([{ operation: op }]);
    expect(text()).toContain(S["ring.declined"]);
    await click("[data-ring-new-code]");
    expect(argsOf("ring_join_start")).toHaveLength(2);
    expect(argsOf("ring_join_start")[1].operation).not.toBe(op);
  });

  it("reconciliation shows its copy with no retry button", async () => {
    setup(status());
    await mount();
    const op = await startJoin();
    await emit("ring:join", joinEvent(op, "reconciliationRequired", { error: "reconciliation_required: x" }));
    expect(text()).toContain(S["ring.reconciliationRequired"]);
    expect(q("[data-ring-new-code]")).toBeNull();
    expect(q("[data-ring-cancel]")).toBeNull();
  });

  it("unmount cancels the shown code", async () => {
    setup(status());
    await mount();
    const op = await startJoin();
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
  });

  it("visible=false (another settings section, hidden Settings) cancels the code", async () => {
    setup(status());
    await mount(true);
    const op = await startJoin();
    await mount(false);
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
    expect(q("[data-ring-words]")).toBeNull();
    expect(q("[data-ring-create]")).not.toBeNull();
  });

  it("pagehide cancels the code", async () => {
    setup(status());
    await mount();
    const op = await startJoin();
    await act(async () => { window.dispatchEvent(new Event("pagehide")); });
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
  });

  it("a start reply after unmount is not rendered and is cancelled", async () => {
    let resolve!: (v: unknown) => void;
    setup(status(), undefined, { ring_join_start: () => new Promise(r => { resolve = r; }) });
    await mount();
    await click("[data-ring-join-action]");
    const op = argsOf("ring_join_start")[0].operation as string;
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    await act(async () => { resolve({ operation: op, words: WORDS, expiresAt: FAR }); });
    expect(container.textContent).not.toContain("acid");
    expect(argsOf("ring_join_cancel").map(a => a.operation)).toContain(op);
  });

  it("a follower window is read-only", async () => {
    setup(status({ connection: "other-window" }), { owner: false, joins: [] });
    await mount();
    expect(text()).toContain(S["ring.follower"]);
    expect(q("[data-ring-create]")).toBeNull();
    expect(q("[data-ring-join-action]")).toBeNull();
  });

  it("platform unavailable says so and disables Create/Join", async () => {
    setup(status(), { owner: true, unavailable: "platform", joins: [] });
    await mount();
    expect(text()).toContain(S["ring.unsupported"]);
    expect(q("[data-ring-create]")!.hasAttribute("disabled")).toBe(true);
    expect(q("[data-ring-join-action]")!.hasAttribute("disabled")).toBe(true);
  });
});

describe("pairing window (xshelld pair on this computer)", () => {
  it("shows the CLI card with Confirm and never the words", async () => {
    const cli = joinEvent("cli-op", "waiting", { origin: "cli" });
    setup(status(), { owner: true, joins: [cli] }, { ring_join_confirm: () => undefined });
    await mount();
    expect(text()).toContain(S["ring.cliStarted"]);
    expect(text()).toContain(S["ring.cliWaiting"]);
    await emit("ring:join", { ...cli, state: "awaitingConfirmation", preview });
    expect(text()).toContain(S["ring.consentTitle"]);
    expect(text()).not.toContain("acid");
    await click("[data-ring-cli] [data-ring-confirm]");
    expect(argsOf("ring_join_confirm")).toEqual([{ operation: "cli-op" }]);
    // A CLI card belongs to the terminal: leaving does not cancel it.
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    expect(argsOf("ring_join_cancel")).toEqual([]);
  });
});

describe("enrolled Desktop: Add computer and Upgrade", () => {
  it("Add computer sends the normalized eight words, never ring_pair_computer, and clears them", async () => {
    setup(enrolled(), undefined, { ring_approve_device_code: () => ({ names: ["laptop", "laptop host"], version: 3 }) });
    await mount();
    const input = q("[data-computer-words]") as HTMLInputElement;
    await act(async () => {
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      set.call(input, "Acid BAKER cable-dance eagle fable gamma habit");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await click("[data-add-computer]");
    expect(argsOf("ring_approve_device_code")).toEqual([{ code: WORDS }]);
    expect(calls()).not.toContain("ring_pair_computer");
    expect(text()).toContain(fmt("mobile.pair.computer.added", { names: "laptop, laptop host" }));
    expect((q("[data-computer-words]") as HTMLInputElement).value).toBe("");
  });

  it("needs-upgrade shows Upgrade Ring and hides Add computer; Upgrade calls ring_upgrade_computer", async () => {
    setup(enrolled({ approval: "needs-upgrade" }), undefined, { ring_upgrade_computer: () => enrolled() });
    await mount();
    expect(q("[data-ring-upgrade]")).not.toBeNull();
    expect(q("[data-add-computer]")).toBeNull();
    await click("[data-ring-upgrade-action]");
    expect(calls()).toContain("ring_upgrade_computer");
    expect(text()).toContain(S["ring.upgrade.done"]);
    expect(q("[data-add-computer]")).not.toBeNull();
  });

  it("an upgrade that cannot run here says why", async () => {
    setup(enrolled({ approval: "unavailable" }));
    await mount();
    expect(text()).toContain(S["ring.upgrade.unavailable"]);
    expect(q("[data-ring-upgrade-action]")).toBeNull();
  });

  it("phone pairing still invokes ring_pair_phone_start (invocation only)", async () => {
    setup(enrolled(), undefined, { ring_pair_phone_start: (a) => ({ operation: a.operation, payload: "xsp1.x", qr: { size: 1, rows: ["1"] }, expiresAt: FAR }), ring_pair_phone_cancel: () => undefined });
    await mount();
    await act(async () => { Array.from(container.querySelectorAll("button")).find(b => b.textContent?.includes(S["mobile.pair.phone"]))!.click(); });
    expect(calls()).toContain("ring_pair_phone_start");
  });
});

describe("review fixes (Sol)", () => {
  it("cancel after Confirm keeps tracking: a later reconciliationRequired shows on return to Hosts", async () => {
    setup(status(), undefined, { ring_join_confirm: () => undefined });
    await mount(true);
    const op = await startJoin();
    await emit("ring:join", joinEvent(op, "awaitingConfirmation", { preview }));
    await click("[data-ring-confirm]");
    await mount(false); // another section: the screen is left
    expect(argsOf("ring_join_cancel")).toEqual([{ operation: op }]);
    await emit("ring:join", joinEvent(op, "reconciliationRequired", { error: "reconciliation_required: x" }));
    await mount(true);
    expect(text()).toContain(S["ring.reconciliationRequired"]);
    expect(q("[data-ring-new-code]")).toBeNull();
    expect(q("[data-ring-create]")).toBeNull();
  });

  it("a persisted unresolved, unpaired Desktop shows the reconcile copy and no Create/Join", async () => {
    setup(status({ approval: "unresolved" }));
    await mount();
    expect(q("[data-ring-unresolved]")?.textContent).toBe(S["ring.reconciliationRequired"]);
    expect(q("[data-ring-create]")).toBeNull();
    expect(q("[data-ring-join-action]")).toBeNull();
  });

  it("Create failing with reconciliation_required goes to reconcile, not a plain failure", async () => {
    setup(status(), undefined, { ring_create_computer: () => { throw "reconciliation_required: an earlier Join is unresolved"; } });
    await mount();
    await click("[data-ring-create]");
    expect(text()).toContain(S["ring.reconciliationRequired"]);
    expect(text()).not.toContain("Couldn’t create the Ring");
    expect(q("[data-ring-create]")).toBeNull();
  });

  it("follower → owner takeover shows Create/Join without remount", async () => {
    setup(status({ connection: "other-window" }), { owner: false, joins: [] });
    await mount();
    expect(text()).toContain(S["ring.follower"]);
    commands.ring_join_overview = () => ({ owner: true, joins: [] });
    await emit("ring:status", status({ connection: "off" }));
    await act(async () => {});
    expect(text()).not.toContain(S["ring.follower"]);
    expect(q("[data-ring-create]")).not.toBeNull();
  });

  it("Create and Join pass the chosen custom Relay; the unpaired form saves it", async () => {
    native.settings.set("ring_relay_url", "wss://own.example.com");
    setup(status(), undefined, { ring_create_computer: () => status() });
    await mount();
    await act(async () => {});
    await click("[data-ring-create]");
    expect(argsOf("ring_create_computer")).toEqual([{ relayUrl: "wss://own.example.com" }]);
    await startJoin();
    expect(argsOf("ring_join_start")[0].relayUrl).toBe("wss://own.example.com");
  });

  it("the unpaired Relay form stores the choice; hosted clears it", async () => {
    setup(status());
    await mount();
    const form = q("[data-ring-relay-unpaired]")!;
    const radios = form.querySelectorAll<HTMLInputElement>("input[type=radio]");
    await act(async () => { radios[1].click(); });
    const input = form.querySelector<HTMLInputElement>("input.edit-input")!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "wss://own.example.com");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => { form.querySelector<HTMLButtonElement>("button.btn-primary")!.click(); });
    expect(native.settings.get("ring_relay_url")).toBe("wss://own.example.com");
    expect(calls()).not.toContain("ring_set_relay_url");
  });

  it("a v2 Ring shows its Relay read-only; a v1 Ring keeps the editable form", async () => {
    setup(enrolled({ format: 2 }));
    await mount();
    expect(q("[data-ring-relay-fixed]")?.textContent).toContain(S["mobile.relay.hosted"]);
    expect(container.querySelector("input[name=mobile-relay]")).toBeNull();
    await act(async () => { root.unmount(); });
    root = createRoot(container);
    setup(enrolled({ format: 1, approval: "needs-upgrade" }));
    await mount();
    expect(q("[data-ring-relay-fixed]")).toBeNull();
    expect(container.querySelector("input[name=mobile-relay]")).not.toBeNull();
  });
});

describe("re-review fixes (Sol)", () => {
  const disabled = (sel: string) => q(sel)!.hasAttribute("disabled");
  const saveRelay = async (url: string) => {
    const form = q("[data-ring-relay-unpaired]")!;
    await act(async () => { form.querySelectorAll<HTMLInputElement>("input[type=radio]")[1].click(); });
    const input = form.querySelector<HTMLInputElement>("input.edit-input")!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, url);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => { form.querySelector<HTMLButtonElement>("button.btn-primary")!.click(); });
  };

  it("an unpaired takeover with no status change shows Create/Join after window focus", async () => {
    setup(status(), { owner: false, joins: [] });
    await mount();
    expect(text()).toContain(S["ring.follower"]);
    commands.ring_join_overview = () => ({ owner: true, joins: [] });
    await act(async () => { window.dispatchEvent(new Event("focus")); });
    await act(async () => {});
    expect(text()).not.toContain(S["ring.follower"]);
    expect(q("[data-ring-create]")).not.toBeNull();
  });

  it("Create/Join are disabled until the saved Relay has loaded", async () => {
    let loaded!: () => void;
    native.gate.get = new Promise(r => { loaded = r; });
    native.settings.set("ring_relay_url", "wss://own.example.com");
    setup(status(), undefined, { ring_create_computer: () => status() });
    await mount();
    expect(disabled("[data-ring-create]")).toBe(true);
    expect(disabled("[data-ring-join-action]")).toBe(true);
    await click("[data-ring-create]");
    expect(calls()).not.toContain("ring_create_computer");
    await act(async () => { loaded(); });
    expect(disabled("[data-ring-create]")).toBe(false);
    await click("[data-ring-create]");
    expect(argsOf("ring_create_computer")).toEqual([{ relayUrl: "wss://own.example.com" }]);
  });

  it("Create/Join are disabled while a Relay Save is pending, then use the new Relay", async () => {
    setup(status(), undefined, { ring_create_computer: () => status() });
    await mount();
    expect(disabled("[data-ring-create]")).toBe(false);
    let saved!: () => void;
    native.gate.set = new Promise(r => { saved = r; });
    await saveRelay("wss://new.example.com");
    expect(disabled("[data-ring-create]")).toBe(true);
    expect(disabled("[data-ring-join-action]")).toBe(true);
    await act(async () => { saved(); });
    expect(disabled("[data-ring-create]")).toBe(false);
    await click("[data-ring-create]");
    expect(argsOf("ring_create_computer")).toEqual([{ relayUrl: "wss://new.example.com" }]);
  });

  it("busy (an admission is held) disables Create/Join without the reconcile copy", async () => {
    setup(status({ busy: true }));
    await mount();
    expect(disabled("[data-ring-create]")).toBe(true);
    expect(disabled("[data-ring-join-action]")).toBe(true);
    expect(q("[data-ring-unresolved]")).toBeNull();
  });
});
