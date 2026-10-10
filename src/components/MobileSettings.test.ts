import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { MembersSection } from "./MobileSettings";
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
    retryIn: null, connectionError: null, limited: false, move: null, problem: null, problemDetail: null,
    members: [me, phone], local: "daemon", hosts: [], ...over,
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
