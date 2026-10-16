import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { cancelPhonePairing, MembersSection, MobileSettingsView, PairingNote, PhonePanel, submitComputer, type Pairing } from "./MobileSettings";
import { COMPUTER_IDLE, PHONE_IDLE, phoneReducer } from "../ring/pairing";
import { fmt } from "../ring/strings";
import { REMOVAL_IDLE, removalReducer, type Removal } from "../ring/mobileSettings";
import type { MemberView, RingStatus } from "../ring/types";

function member(over: Partial<MemberView>): MemberView {
  return { name: "m", role: "daemon", signKey: "k", thisApp: false, thisComputer: false, hostId: null, removable: false, presence: { kind: "online" }, ...over };
}

const me = member({ name: "desk", role: "desktop", signKey: "d", thisApp: true });
const phone = member({ name: "Pixel", role: "mobile", signKey: "p", removable: true });

function status(over: Partial<RingStatus> = {}): RingStatus {
  return {
    enabled: true, ringId: "r", version: 2, relayUrl: "wss://r", hostedRelayUrl: "wss://r", connection: "connected",
    retryIn: null, connectionError: null, limited: false, farewells: [], problem: null, problemDetail: null,
    members: [me, phone], quotaResetAt: null, relayHosted: true, local: "daemon", hosts: [], ...over,
  };
}

const render = (s: RingStatus, removal: Removal) =>
  renderToStaticMarkup(createElement(MembersSection, { s, removal, onRemove: () => {}, onClaimHost: async () => {} }));

describe("the device list", () => {
  it("offers Remove on a phone, not on this app", () => {
    const html = render(status(), REMOVAL_IDLE);
    expect(html.match(/host-row-remove/g)?.length).toBe(1);
    expect(html).toContain(fmt("mobile.member.remove"));
  });

  it("shows Removing… on the row being removed", () => {
    const removing = removalReducer(removalReducer(REMOVAL_IDLE, { type: "ask", member: phone }), { type: "start" });
    const html = render(status(), removing);
    expect(html).toContain(fmt("mobile.member.removing"));
  });

  it("keeps the pending note after the removed row is gone", () => {
    let r = removalReducer(REMOVAL_IDLE, { type: "ask", member: phone });
    r = removalReducer(r, { type: "start" });
    const after = status({ connection: "waiting", members: [me] });
    r = removalReducer(r, { type: "done", status: after });
    const html = render(after, r);
    expect(html).not.toContain(">Pixel<");
    expect(html).toContain(fmt("mobile.remove.pending", { name: "Pixel" }));
  });

  it("hides Remove in a window that does not run the connection", () => {
    expect(render(status({ connection: "other-window" }), REMOVAL_IDLE)).not.toContain("host-row-remove");
  });
});

describe("pairing and the connection", () => {
  const pairing = (pairComputer = vi.fn(async () => {})): Pairing => ({
    events: { phone: null, computer: null },
    startPhone: async () => { throw new Error("not here"); },
    cancelPhone: async () => {},
    pairComputer,
    cancelComputer: async () => {},
  });
  const view = (s: RingStatus) =>
    renderToStaticMarkup(createElement(MobileSettingsView, {
      s, onEnable: async () => {}, onSaveRelay: async () => {}, onClaimHost: async () => {},
      onRemoveMember: async () => s, pairing: pairing(),
    }));
  // Text as the markup escapes it.
  const html = (t: string) => t.replace(/&/g, "&amp;").replace(/'/g, "&#x27;");
  // The "Pair a phone" button: the one with its label.
  const phoneButton = (html: string) => {
    const m = new RegExp(`<button[^>]*>(?:(?!</button>).)*${fmt("mobile.pair.phone")}</button>`, "s").exec(html);
    expect(m, "the Pair a phone button").not.toBeNull();
    return m![0];
  };

  it("disables pairing with a note while the relay can't be reached or this app was removed", () => {
    for (const [connection, key] of [["waiting", "mobile.pair.needsRelay"], ["stopped", "mobile.pair.needsMember"]] as const) {
      const out = view(status({ connection, retryIn: 3 }));
      expect(phoneButton(out), connection).toContain("disabled");
      // Both panels carry the note.
      expect(out.split(html(fmt(key))).length - 1, connection).toBe(2);
    }
  });

  it("offers pairing while connecting, noting it waits for the connection", () => {
    const out = view(status({ connection: "connecting" }));
    expect(phoneButton(out)).not.toContain("disabled");
    expect(out.split(html(fmt("mobile.pair.connecting"))).length - 1).toBe(2);
    const connected = view(status());
    expect(phoneButton(connected)).not.toContain("disabled");
    expect(connected).not.toContain(html(fmt("mobile.pair.connecting")));
  });

  it("keeps a shown phone offer waiting for the connection during a retry", () => {
    const note = (s: RingStatus, offerShown: boolean) => renderToStaticMarkup(createElement(PairingNote, { s, offerShown }));
    const backoff = status({ connection: "waiting", retryIn: 4 });
    const shown = note(backoff, true);
    expect(shown).toContain(html(fmt("mobile.pair.connecting")));
    expect(shown).not.toContain(html(fmt("mobile.pair.needsRelay")));
    expect(shown).not.toContain("host-row-warn");
    // Without an offer, pairing cannot start now.
    const idle = note(backoff, false);
    expect(idle).toContain(html(fmt("mobile.pair.needsRelay")));
    expect(idle).toContain("host-row-warn");
  });

  it("hides pairing in a window that does not run the connection", () => {
    expect(view(status({ connection: "other-window" }))).not.toContain(fmt("mobile.pair.computer.label"));
  });

  it("does not add a computer on Enter while pairing is blocked", async () => {
    const code = "0123-4567-89AB-CDEF";
    for (const connection of ["waiting", "stopped"] as const) {
      const pairComputer = vi.fn(async () => {});
      const dispatch = vi.fn();
      await submitComputer(code, COMPUTER_IDLE, status({ connection }), pairComputer, dispatch);
      expect(pairComputer, connection).not.toHaveBeenCalled();
      expect(dispatch, connection).not.toHaveBeenCalled();
    }
    const pairComputer = vi.fn(async () => {});
    const dispatch = vi.fn();
    await submitComputer(code, COMPUTER_IDLE, status({ connection: "connecting" }), pairComputer, dispatch);
    expect(pairComputer).toHaveBeenCalledWith("0123456789ABCDEF");
    expect(dispatch).toHaveBeenCalledWith({ type: "submit" });
  });
});

describe("the quota notice (#42)", () => {
  const view = (s: RingStatus) =>
    renderToStaticMarkup(createElement(MobileSettingsView, {
      s, onEnable: async () => {}, onSaveRelay: async () => {}, onClaimHost: async () => {},
      onRemoveMember: async () => s,
    }));
  const html = (t: string) => t.replace(/&/g, "&amp;").replace(/'/g, "&#x27;");
  const resetAt = Math.floor(Date.now() / 1000) + 3600;
  const hosted = (time: string) => html(fmt("mobile.quota.hosted", { time }));
  const time = new Date(resetAt * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

  it("shows the quota notice while the Ring is quota-limited", () => {
    const out = view(status({ quotaResetAt: resetAt }));
    expect(out).toContain(hosted(time));
    expect(out).toContain("host-row-note host-row-warn");
  });

  it("names your own relay on a self-hosted relay", () => {
    expect(view(status({ quotaResetAt: resetAt, relayHosted: false }))).toContain(html(fmt("mobile.quota.ownRelay", { time })));
  });

  it("clears the quota notice", () => {
    const out = view(status({ quotaResetAt: null }));
    expect(out).not.toContain("daily message limit");
    expect(view(status({ quotaResetAt: Math.floor(Date.now() / 1000) - 1 }))).not.toContain("daily message limit");
  });
});

describe("farewell lines in the view", () => {
  const view = (s: RingStatus) => renderToStaticMarkup(createElement(MobileSettingsView, {
    s, onEnable: async () => {}, onSaveRelay: async () => {}, onClaimHost: async () => {}, onRemoveMember: async () => s,
  }));

  it("shows a note per old relay, warning once a retry failed", () => {
    const html = view(status({ farewells: [
      { relayUrl: "wss://a.example", state: "pending", failures: 0 },
      { relayUrl: "wss://b.example", state: "waiting", failures: 2 },
    ] }));
    expect(html).toContain(fmt("mobile.farewell.owed", { relay: "a.example" }));
    expect(html).toContain(`host-row-warn">${fmt("mobile.farewell.owed", { relay: "b.example" })}`);
    expect(html).not.toContain(`host-row-warn">${fmt("mobile.farewell.owed", { relay: "a.example" })}`);
  });

  it("names the hosted relay under the Hosted option", () => {
    expect(view(status({ hostedRelayUrl: "wss://hosted.example" }))).toContain("hosted.example");
  });
});

describe("Cancel pairing", () => {
  const offer = { payload: "p", expiresAt: Date.now() + 600_000, qr: { size: 1, rows: ["1"] } } as never;
  const pairing = (cancelPhone = vi.fn(async () => {})): Pairing => ({
    events: { phone: null, computer: null },
    startPhone: async () => offer,
    cancelPhone,
    pairComputer: async () => {},
    cancelComputer: async () => {},
  });
  const panel = (initial: Parameters<typeof PhonePanel>[0]["initial"]) =>
    renderToStaticMarkup(createElement(PhonePanel, { s: status(), p: pairing(), initial }));

  it("is offered with the QR code, not before", () => {
    expect(panel({ state: "waiting", offer })).toContain(fmt("mobile.pair.phone.cancel"));
    expect(panel(PHONE_IDLE)).not.toContain(fmt("mobile.pair.phone.cancel"));
  });

  it("calls the cancel and the view returns to Pair a phone", async () => {
    const cancel = vi.fn(async () => {});
    let st = phoneReducer(phoneReducer(PHONE_IDLE, { type: "start" }), { type: "started", offer });
    expect(st.state).toBe("waiting");
    await cancelPhonePairing(cancel, a => { st = phoneReducer(st, a); });
    expect(cancel).toHaveBeenCalledOnce();
    expect(st).toEqual(PHONE_IDLE);
    const html = panel(st);
    expect(html).toContain(">Pair a phone<");
    expect(html).not.toContain(fmt("mobile.pair.phone.cancel"));
  });
});
