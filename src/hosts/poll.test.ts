import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { pollDue, RELAY_POLL_EVERY, startPoll, viaRelay } from "./poll";
import type { RegistrySnapshot } from "./registry";
import type { AvailableHost } from "./types";

const row = (id: string, activeRoute: AvailableHost["activeRoute"], aliases: string[] = []): AvailableHost =>
  ({ id, aliases, name: id, color: null, daemonKey: null, routes: [activeRoute], activeRoute, reachability: { kind: "online" } });
const snap = { available: [row("r_relay", "relay", ["h_moved000"]), row("h_ssh00000", "ssh")] } as unknown as RegistrySnapshot;

function fakeDoc() {
  const listeners = new Set<() => void>();
  const doc = {
    visibilityState: "visible" as DocumentVisibilityState,
    addEventListener: (_: string, f: () => void) => { listeners.add(f); },
    removeEventListener: (_: string, f: () => void) => { listeners.delete(f); },
  };
  const show = (v: DocumentVisibilityState) => { doc.visibilityState = v; for (const f of listeners) f(); };
  return { doc: doc as unknown as Document, show, listeners };
}

describe("polls that read from a Host", () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); });

  it("only a Host whose active route is the Relay travels the Relay", () => {
    expect(viaRelay("r_relay", snap)).toBe(true);
    expect(viaRelay("h_moved000", snap)).toBe(true);
    expect(viaRelay("h_ssh00000", snap)).toBe(false);
    expect(viaRelay(undefined, snap)).toBe(false);
    expect(viaRelay("h_unknown0", snap)).toBe(false);
  });

  it("a Relay Host is due on every fourth tick, the others on each", () => {
    const due = (h: string | undefined) => [1, 2, 3, 4, 5, 6, 7, 8].filter(n => pollDue(h, n, snap));
    expect(due("r_relay")).toEqual([4, 8]);
    expect(due("h_ssh00000")).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
    expect(due(undefined)).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
    // The first poll and a catch-up ask everyone.
    expect(pollDue("r_relay", 0, snap)).toBe(true);
    expect(pollDue("r_relay", RELAY_POLL_EVERY, snap)).toBe(true);
  });

  it("does not tick while hidden and catches up once when shown", () => {
    const { doc, show, listeners } = fakeDoc();
    const seen: number[] = [];
    const stop = startPoll(n => seen.push(n), 1000, undefined, doc);
    vi.advanceTimersByTime(2000);
    expect(seen).toEqual([1, 2]);
    show("hidden");
    vi.advanceTimersByTime(5000);
    expect(seen).toEqual([1, 2]);
    show("visible");
    expect(seen).toEqual([1, 2, RELAY_POLL_EVERY]);
    show("visible");
    expect(seen.length).toBe(3);
    vi.advanceTimersByTime(1000);
    expect(seen).toEqual([1, 2, RELAY_POLL_EVERY, 3]);
    stop();
    vi.advanceTimersByTime(3000);
    expect(seen.length).toBe(4);
    expect(listeners.size).toBe(0);
  });

  it("shown again with no tick missed: nothing extra", () => {
    const { doc, show } = fakeDoc();
    const seen: number[] = [];
    const stop = startPoll(n => seen.push(n), 1000, undefined, doc);
    show("hidden");
    show("visible");
    expect(seen).toEqual([]);
    stop();
  });
});
