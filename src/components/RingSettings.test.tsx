// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RingSettings } from "./RingSettings";
import { C, fmtc } from "../ring/copy";
import { FAR, WORDS, me, member, phone, ringStatus, tower } from "./ringTestKit";
import type { RingStatus } from "../ring/types";
import type { AvailableHost, HostStatus } from "../hosts/types";

const native = vi.hoisted(() => ({
  invoke: vi.fn(), handlers: new Map<string, (event: { payload: unknown }) => void>(), settings: new Map<string, unknown>(),
  snap: { configs: [], available: [] as Partial<AvailableHost>[], status: {} as Record<string, Partial<HostStatus>>, live: {}, agents: {} },
  slots: [] as { host: { id: string; name: string }; status: unknown }[],
  all: 0,
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: async () => ({
  get: async (k: string) => native.settings.get(k) ?? null,
  set: async (k: string, v: unknown) => { native.settings.set(k, v); },
  save: async () => {},
}) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: unknown }) => void) => { native.handlers.set(name, handler); return () => native.handlers.delete(name); } }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => native.snap }));
// S4's components arrive through this seam; their props are what is asserted.
vi.mock("../ring/updateSlots", () => ({
  MachineUpdateSlot: (p: { host: { id: string; name: string }; status: unknown }) => { native.slots.push(p); return null; },
  UpdateAllSlot: () => { native.all++; return null; },
}));

let container: HTMLDivElement;
let root: Root;
let commands: Record<string, (args: Record<string, unknown>) => unknown>;
let onOpen: ReturnType<typeof vi.fn>;
const argsOf = (command: string) => native.invoke.mock.calls.filter(([c]) => c === command).map(([, a]) => a as Record<string, unknown>);

function setup(s: RingStatus, extra: typeof commands = {}, owner = true) {
  commands = { ring_status: () => s, ring_join_overview: () => ({ owner, joins: [] }), ...extra };
  native.invoke.mockImplementation(async (command: string, args: Record<string, unknown>) => {
    const f = commands[command];
    if (!f) throw new Error(`unexpected ${command}`);
    return f(args ?? {});
  });
}
async function mount() { await act(async () => { root.render(<RingSettings onOpenRingSetup={onOpen} />); }); }
const emit = async (name: string, payload: unknown) => { await act(async () => { native.handlers.get(name)!({ payload }); }); };
const q = (sel: string) => container.querySelector<HTMLElement>(sel);
const click = async (sel: string) => { await act(async () => { q(sel)!.click(); }); };
const text = () => container.textContent ?? "";
const dialogButton = (label: string) => Array.from(container.querySelectorAll<HTMLButtonElement>("[role=dialog] button")).find(b => b.textContent === label)!;
async function type(sel: string, value: string) {
  const input = q(sel) as HTMLInputElement;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  native.invoke.mockReset();
  native.handlers.clear();
  native.settings.clear();
  native.snap.available = [];
  native.snap.status = {};
  native.slots = [];
  native.all = 0;
  onOpen = vi.fn();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => { root.unmount(); });
  container.remove();
  vi.unstubAllGlobals();
});

describe("member list", () => {
  it("Remove button on every row incl. This machine, accessible name Remove <name>", async () => {
    setup(member());
    await mount();
    expect(text()).toContain(C["ring.members"]);
    const buttons = Array.from(container.querySelectorAll<HTMLButtonElement>("[data-member-remove]"));
    expect(buttons.map(b => b.getAttribute("data-member-remove"))).toEqual(["me", "tw", "ph"]);
    expect(buttons.map(b => b.getAttribute("aria-label"))).toEqual(["Remove desk", "Remove tower", "Remove Pixel"]);
    expect(buttons.every(b => b.textContent?.includes(C["member.remove"]))).toBe(true);
    const mine = q('[data-ring-member="me"]')!;
    expect(mine.textContent).toContain(`${C["member.machine"]} · ${C["member.this"]}`);
    expect(q('[data-ring-member="ph"]')!.textContent).toContain(C["member.phone"]);
  });

  it("self-remove sends own signKey, then shows remove.self.done and Set up your Ring", async () => {
    setup(member(), { ring_remove_member: () => ringStatus({ membership: "none" }) });
    await mount();
    await click('[aria-label="Remove desk"]');
    expect(q("[role=dialog]")?.textContent).toContain(C["remove.self.title"]);
    expect(q("[role=dialog]")?.textContent).toContain(C["remove.self.body"]);
    await act(async () => { dialogButton(C["remove.action"]).click(); });
    expect(argsOf("ring_remove_member")).toEqual([{ signKey: "me" }]);
    expect(text()).toContain(C["remove.self.done"]);
    expect(q("[data-ring-setup-open]")?.textContent).toBe(C["first.settings"]);
    await click("[data-ring-setup-open]");
    expect(onOpen).toHaveBeenCalledOnce();
  });

  it("removing another machine names it; Cancel removes nothing", async () => {
    setup(member(), { ring_remove_member: () => member({ members: [me, phone] }) });
    await mount();
    await click('[aria-label="Remove tower"]');
    expect(q("[role=dialog]")?.textContent).toContain(fmtc("remove.other.body", { name: "tower" }));
    await act(async () => { dialogButton(C["remove.cancel"]).click(); });
    expect(argsOf("ring_remove_member")).toEqual([]);
    await click('[aria-label="Remove tower"]');
    await act(async () => { dialogButton(C["remove.action"]).click(); });
    expect(text()).toContain(fmtc("remove.done", { name: "tower" }));
    expect(q('[data-ring-member="tw"]')).toBeNull();
  });

  it("the last machine gets remove.last", async () => {
    setup(member({ members: [me, phone] }));
    await mount();
    await click('[aria-label="Remove desk"]');
    expect(q("[role=dialog]")?.textContent).toContain(C["remove.last.title"]);
  });

  it("removal offline: remove.offline shown, member still listed", async () => {
    setup(member(), { ring_remove_member: () => { throw "offline: relay unreachable"; } });
    await mount();
    await click('[aria-label="Remove tower"]');
    await act(async () => { dialogButton(C["remove.action"]).click(); });
    expect(text()).toContain(C["remove.offline"]);
    expect(text()).not.toContain("relay unreachable");
    expect(q('[data-ring-member="tw"]')).not.toBeNull();
  });

  it("uncertain offers Retry, which re-reads membership before removing again", async () => {
    let n = 0;
    setup(member(), { ring_remove_member: () => { if (n++ === 0) throw "uncertain: no answer"; return member({ members: [me, phone] }); } });
    await mount();
    await click('[aria-label="Remove tower"]');
    await act(async () => { dialogButton(C["remove.action"]).click(); });
    expect(text()).toContain(C["remove.uncertain"]);
    await click("[data-remove-retry]");
    const order = native.invoke.mock.calls.map(([c]) => c).filter(c => c === "ring_status" || c === "ring_remove_member");
    expect(order.slice(-2)).toEqual(["ring_status", "ring_remove_member"]);
    expect(text()).toContain(fmtc("remove.done", { name: "tower" }));
  });

  it("a follower window cannot remove and says why", async () => {
    setup(member({ connection: "other-window" }), {}, false);
    await mount();
    expect(Array.from(container.querySelectorAll<HTMLButtonElement>("[data-member-remove]")).every(b => b.disabled)).toBe(true);
    expect(text()).toContain(C["ring.follower"]);
    expect(q("[data-computer-words]")).toBeNull();
  });

  it("presence and version: from the Relay and the matched Host's status", async () => {
    native.snap.available = [{ id: "r_tower", daemonKey: "twk" }];
    native.snap.status = { r_tower: { daemonVersion: "1.6.0" }, local: { daemonVersion: "1.6.1" } };
    setup(member({ members: [me, { ...tower, presence: { kind: "closed" }, daemonPresence: { kind: "unreachable" } }, phone] }));
    await mount();
    expect(q('[data-ring-member="tw"]')!.textContent).toContain("Version 1.6.0");
    expect(q('[data-ring-member="tw"]')!.textContent).toContain(C["member.offline"]);
    expect(q('[data-ring-member="me"]')!.textContent).toContain("Version 1.6.1");
    expect(q('[data-ring-member="ph"]')!.textContent).not.toContain("Version");
  });

  it("Ring machine row mounts HostUpdateAction with the Host id matched by daemonKey; HostUpdateAll mounted once", async () => {
    native.snap.available = [{ id: "r_tower", daemonKey: "twk" }];
    native.snap.status = { r_tower: { daemonVersion: "1.6.0", host: "r_tower" } };
    setup(member());
    await mount();
    expect(native.all).toBeGreaterThan(0);
    // One slot per render pass, only for tower: not this machine, not the phone.
    const hosts = new Set(native.slots.map(s => JSON.stringify(s.host)));
    expect([...hosts]).toEqual([JSON.stringify({ id: "r_tower", name: "tower" })]);
    expect(native.slots[native.slots.length - 1].status).toEqual({ daemonVersion: "1.6.0", host: "r_tower" });
    expect(container.querySelectorAll("[data-ring-members]")).toHaveLength(1);
  });
});

describe("Add a machine", () => {
  it("while the service cannot be reached it says so (default service, no address) and is disabled", async () => {
    setup(member({ connection: "waiting", retryIn: 1 }));
    await mount();
    expect(q("[data-ring-connection]")?.textContent).toBe("Can't reach the service. Retrying in 1 second.");
    expect(q("[data-add-machine] [data-ring-connection-required]")?.textContent).toBe(C["ring.connection.default.required"]);
    await type("[data-computer-words]", WORDS);
    expect((q("[data-add-computer]") as HTMLButtonElement).disabled).toBe(true);
    expect(text().toLowerCase()).not.toContain("relay");
  });

  it("add.format for 9 words; sends normalized words; invalid_code → add.invalid; closed → add.closed; done → Added X", async () => {
    let reply: () => unknown = () => { throw "invalid_code: not a code"; };
    setup(member(), { ring_approve_device_code: () => reply() });
    await mount();
    expect(text()).toContain(C["add.entry"]);
    await type("[data-computer-words]", `${WORDS} ivory`);
    expect(text()).toContain(C["add.format"]);
    expect((q("[data-add-computer]") as HTMLButtonElement).disabled).toBe(true);
    await type("[data-computer-words]", "Acid BAKER cable-dance eagle fable gamma habit");
    expect(text()).not.toContain(C["add.format"]);
    await click("[data-add-computer]");
    expect(argsOf("ring_approve_device_code")).toEqual([{ code: WORDS }]);
    expect(text()).toContain(C["add.invalid"]);
    reply = () => { throw "closed: the other machine left its code screen"; };
    await click("[data-add-computer]");
    expect(text()).toContain(C["add.closed"]);
    reply = () => ({ names: ["laptop"], version: 3 });
    await click("[data-add-computer]");
    expect(text()).toContain("Added laptop to your Ring.");
    expect((q("[data-computer-words]") as HTMLInputElement).value).toBe("");
  });

  it("maps every approve code to approved copy, never the raw error", async () => {
    const cases: [string, string][] = [["not_found: x", C["add.invalid"]], ["expired: x", C["add.invalid"]], ["offline: x", C["add.offline"]], ["already_used: x", C["add.alreadyUsed"]], ["busy: x", C["add.busy"]], ["not_approver: x", C["add.notApprover"]], ["finishing: x", C["add.finishing"]], ["other_window: x", C["add.otherWindow"]], ["uncertain: x", C["add.failed"]], ["other: boom", C["add.failed"]]];
    for (const [error, line] of cases) {
      setup(member(), { ring_approve_device_code: () => { throw error; } });
      await mount();
      await type("[data-computer-words]", WORDS);
      await click("[data-add-computer]");
      expect(text(), error).toContain(line);
      expect(text(), error).not.toContain(": x");
      await act(async () => { root.unmount(); });
      root = createRoot(container);
    }
  });
});

describe("status ordering", () => {
  it("a ring_status reply older than a ring:status event does not overwrite it", async () => {
    let reply!: (s: RingStatus) => void;
    setup(ringStatus(), { ring_status: () => new Promise(r => { reply = r; }) });
    await mount();
    await emit("ring:status", member());
    expect(q("[data-ring-members]")).not.toBeNull();
    await act(async () => { reply(ringStatus({ membership: "finishing" })); });
    expect(q("[data-ring-members]")).not.toBeNull();
    expect(text()).not.toContain(C["finish.title"]);
  });

  it("a command reply older than an event does not overwrite it (Remove answers after a removal event)", async () => {
    let reply!: (s: RingStatus) => void;
    setup(member(), { ring_remove_member: () => new Promise(r => { reply = r; }) });
    await mount();
    await click('[aria-label="Remove tower"]');
    await act(async () => { dialogButton(C["remove.action"]).click(); });
    await emit("ring:status", ringStatus({ membership: "removed", removed: { ringId: "r", at: 8 } }));
    await act(async () => { reply(member({ members: [me, phone] })); });
    expect(text()).toContain(C["removed.title"]);
    expect(q("[data-ring-members]")).toBeNull();
  });
});

describe("other memberships", () => {
  it("finishing shows Finishing… with no buttons", async () => {
    setup(ringStatus({ membership: "finishing" }));
    await mount();
    expect(text()).toContain(C["finish.title"]);
    expect(text()).toContain(C["finish.body"]);
    expect(container.querySelectorAll("button")).toHaveLength(0);
  });

  it("removed shows the notice with Join again (starts the words)", async () => {
    setup(ringStatus({ membership: "removed", removed: { ringId: "r", at: 3 } }));
    await mount();
    expect(text()).toContain(C["removed.title"]);
    await click("[data-ring-join-again]");
    expect(onOpen).toHaveBeenCalledWith("join");
  });

  it("a removal arriving while Settings → Ring is open switches it at once (U3)", async () => {
    setup(member());
    await mount();
    expect(q("[data-ring-members]")).not.toBeNull();
    await emit("ring:status", ringStatus({ membership: "removed", removed: { ringId: "r", at: 4 } }));
    expect(q("[data-ring-members]")).toBeNull();
    expect(text()).toContain(C["removed.title"]);
  });

  it("not in a Ring: Set up your Ring, and the self-hosted Relay chosen before Create", async () => {
    setup(ringStatus());
    await mount();
    await click("[data-ring-setup-open]");
    expect(onOpen).toHaveBeenCalledWith();
    expect(text()).toContain(C["ring.relay.label"]);
    expect(text()).toContain(C["ring.relay.help"]);
    await type("[data-ring-relay-unpaired] input", "wss://own.example.com");
    await act(async () => { q("[data-ring-relay-unpaired] button")!.click(); });
    expect(native.settings.get("ring_relay_url")).toBe("wss://own.example.com");
  });

  it("no dead UI for any membership", async () => {
    const statuses = [ringStatus(), member(), ringStatus({ membership: "finishing" }), ringStatus({ membership: "removed", removed: { ringId: "r", at: 1 } }), ringStatus({ oldRing: { at: 2 } })];
    for (const s of statuses) {
      setup(s);
      await mount();
      for (const dead of ["Upgrade Ring", "Leave this Ring", "Decline", "Join this Ring?", "Pair with this desktop", "keeps trying to reach", "Enable mobile access", "inside the app"]) {
        expect(text(), `${s.membership}: ${dead}`).not.toContain(dead);
      }
      await act(async () => { root.unmount(); });
      root = createRoot(container);
    }
  });
});
