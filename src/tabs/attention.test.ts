import { describe, expect, it } from "vitest";
import type { Tab, Group } from "../types";
import type { TerminalInfo } from "../hosts/types";
import { attentionKey, attentionRing, decodeCompletionSeen, markCompletionSeen, persistedCompletionSeen, pruneCompletionSeen, removeCompletionSeenHost, visibleTerminalIds } from "./attention";

const tab: Tab = { id: "a", type: "terminal", title: "A", host: "h_aaaaaaaa", terminal: "terminal" };
const f = "00000000-0000-4000-8000-000000000001", g = "00000000-0000-4000-8000-000000000002";
const key = attentionKey(tab);
const info = { terminal: "terminal", exitCode: null, completionAttention: { completionId: f, inputHandled: false } } as TerminalInfo;

describe("Desktop completion attention", () => {
  it.each([
    { name: "Working even while viewed", status: "working", viewing: true, expected: "working" },
    { name: "needs-you", status: "needs-you", expected: "needs-you" },
    { name: "unread finished", status: "finished", expected: "unread" },
    { name: "seen finished", status: "finished", seen: f, expected: null },
    { name: "inputHandled finished", status: "finished", handled: true, expected: null },
    { name: "viewed finished", status: "finished", viewing: true, expected: null },
    { name: "stale Working", status: "working", stale: true, expected: null },
    { name: "closing Working", status: "working", closing: true, expected: null },
    { name: "exited Working", status: "working", exited: true, expected: null },
  ] as const)("preserves presentation-independent attention: $name", row => {
    const c = { stale: false, closing: false, exited: false, handled: false, viewing: false, seen: undefined, ...row };
    expect(attentionRing({ status: c.status, stale: c.stale }, {
      ...info, exitCode: c.exited ? 0 : null,
      completionAttention: { completionId: f, inputHandled: c.handled },
    }, c.seen, f, c.viewing, c.closing)).toBe(c.expected);
  });
  it("keeps two independent local seen stores; only shared handled clears the other", () => {
    const a = markCompletionSeen({}, key, f, f), b = {};
    expect(attentionRing({ status: "finished", stale: false }, info, a[key], f, false, false)).toBeNull();
    expect(attentionRing({ status: "finished", stale: false }, info, b[key as keyof typeof b], f, false, false)).toBe("unread");
    expect(attentionRing({ status: "finished", stale: false }, { ...info, completionAttention: { completionId: f, inputHandled: true } }, undefined, f, false, false)).toBeNull();
    expect(attentionRing({ status: "working", stale: false }, info, f, f, true, false)).toBe("working");
    expect(attentionRing({ status: "finished", stale: false }, { ...info, completionAttention: { completionId: g, inputHandled: false } }, a[key], g, false, false)).toBe("unread");
  });
  it("does not overwrite newer seen from a stale callback, use clock marks, or invent missing tokens", () => {
    const seen = { [key]: g };
    expect(markCompletionSeen(seen, key, f, g)).toBe(seen);
    expect(markCompletionSeen(seen, key, undefined, g)).toBe(seen);
    expect(markCompletionSeen(seen, key, g, g)).toBe(seen);
    expect(decodeCompletionSeen({ version: 1, entries: { [key]: Number.MAX_SAFE_INTEGER } })).toEqual({});
    expect(attentionRing({ status: "finished", stale: false }, { ...info, statusAtMs: 9999999999, completionAttention: undefined }, undefined, undefined, false, false)).toBeNull();
    expect(decodeCompletionSeen({ version: 1, entries: { [key]: f } })).toEqual({ [key]: f });
    expect(attentionKey({ ...tab, host: "h_bbbbbbbb" })).not.toBe(key);
  });
  it("retains offline marks, prunes only authoritative removals/Host removal, never persists in-process observations", () => {
    const fallback = attentionKey({ id: "local", title: "Local", type: "terminal" });
    const seen = { [key]: f, [fallback]: "local:12" };
    expect(pruneCompletionSeen(seen, tab.host!, null)).toBe(seen);
    expect(pruneCompletionSeen(seen, tab.host!, ["terminal"])).toBe(seen);
    expect(pruneCompletionSeen(seen, tab.host!, [])).toEqual({ [fallback]: "local:12" });
    expect(removeCompletionSeenHost(seen, tab.host!)).toEqual({ [fallback]: "local:12" });
    expect(persistedCompletionSeen(seen)).toEqual({ [key]: f });
    expect(decodeCompletionSeen({ version: 1, entries: seen })).toEqual({ [key]: f });
  });
  it("observes all active Group leaves, not only the focused leaf; Home/Settings and parked leaves are absent", () => {
    const tabs = [tab, { ...tab, id: "b", groupId: "group" }, { ...tab, id: "c", groupId: "group" }, { ...tab, id: "parked", groupId: "other" }];
    const groups: Group[] = [{ id: "group", name: "Group", layout: { kind: "split", direction: "col", ratio: .5, children: [{ kind: "leaf", tabId: "b" }, { kind: "leaf", tabId: "c" }] } }];
    expect([...visibleTerminalIds(tabs, groups, "group")]).toEqual(["b", "c"]);
    expect([...visibleTerminalIds(tabs, groups, "a")]).toEqual(["a"]);
    for (const active of ["home", "settings", "missing", "b"]) expect(visibleTerminalIds(tabs, groups, active).size).toBe(0);
  });
});
