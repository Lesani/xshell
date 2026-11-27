import { describe, expect, it } from "vitest";
import { fmtc } from "./copy";
import { hostOf, kindKey, presenceChip, removeDialog, removeErrorKey, versionLine } from "./members";
import type { MemberView } from "./types";
import type { HostStatus } from "../hosts/types";

const m = (over: Partial<MemberView>): MemberView => ({
  name: "box", role: "desktop", signKey: "s", thisApp: false, thisComputer: false, hostId: null, removable: true,
  presence: { kind: "online" }, enrollmentId: "u", kind: "computer", daemonKey: "d", daemonPresence: null, ...over,
});
const me = m({ name: "desk", signKey: "me", thisApp: true, thisComputer: true });
const tower = m({ name: "tower", signKey: "t", daemonKey: "tk" });
const head = m({ name: "vm", signKey: "h", role: "daemon", kind: "daemon", daemonKey: null });
const phone = m({ name: "Pixel", signKey: "p", role: "mobile", kind: "mobile", daemonKey: null });

describe("Ring members", () => {
  it("labels: Machine, Machine without a window, Phone", () => {
    expect([kindKey(tower), kindKey(head), kindKey(phone)].map(k => fmtc(k))).toEqual(["Machine", "Machine without a window", "Phone"]);
    expect(kindKey(m({ kind: null, role: "daemon" }))).toBe("member.headless");
  });

  it("presence: online when either key is; no chip for others while not connected", () => {
    expect(presenceChip(m({ presence: { kind: "closed" }, daemonPresence: { kind: "online" } }), "connected")).toBe("member.online");
    expect(presenceChip(m({ presence: { kind: "unreachable" } }), "connected")).toBe("member.offline");
    expect(presenceChip(tower, "waiting")).toBeNull();
    expect(presenceChip(me, "waiting")).toBe("member.online");
  });

  it("dialog: self → remove.self; phone → remove.phone; the only machine (phones present) → remove.last; other → remove.other with name", () => {
    const all = [me, tower, phone];
    expect(removeDialog(me, all).title).toBe(fmtc("remove.self.title"));
    expect(removeDialog(me, all).body).toBe(fmtc("remove.self.body"));
    expect(removeDialog(me, all).self).toBe(true);
    expect(removeDialog(phone, all)).toEqual({ title: "Remove Pixel?", body: fmtc("remove.phone.body", { name: "Pixel" }), self: false });
    expect(removeDialog(me, [me, phone]).title).toBe("Remove the last machine?");
    expect(removeDialog(tower, all)).toEqual({ title: "Remove tower?", body: fmtc("remove.other.body", { name: "tower" }), self: false });
  });

  it("errors: offline/conflict/uncertain and the gaps → their keys, never raw text", () => {
    expect(removeErrorKey("offline")).toBe("remove.offline");
    expect(removeErrorKey("conflict")).toBe("remove.changed");
    expect(removeErrorKey("uncertain")).toBe("remove.uncertain");
    expect(removeErrorKey("busy")).toBe("remove.busy");
    expect(removeErrorKey("other_window")).toBe("remove.otherWindow");
    expect(removeErrorKey("not_enabled")).toBe("remove.notMember");
    expect(removeErrorKey("other")).toBe("remove.failed");
  });

  it("version from the matched Host status, else Version unknown; phones none", () => {
    const available = [{ id: "r_1", daemonKey: "tk" }, { id: "r_2", daemonKey: "zz" }];
    expect(hostOf(tower, available)).toBe("r_1");
    expect(hostOf(head, [{ id: "r_3", daemonKey: "h" }])).toBe("r_3");
    expect(hostOf(m({ daemonKey: "nope" }), available)).toBeNull();
    expect(versionLine(tower, { daemonVersion: "1.6.0" } as HostStatus)).toBe("Version 1.6.0");
    expect(versionLine(tower, undefined)).toBe("Version unknown");
    expect(versionLine(phone, { daemonVersion: "1.6.0" } as HostStatus)).toBeNull();
  });
});
