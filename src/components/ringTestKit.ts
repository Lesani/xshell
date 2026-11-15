// Shared fixtures for the Ring component tests (native boundary mocked by each test file).
import type { MemberView, RingStatus } from "../ring/types";

export const WORDS = "acid-baker-cable-dance-eagle-fable-gamma-habit";
export const FAR = Math.floor(Date.now() / 1000) + 600;

export function ringStatus(over: Partial<RingStatus> = {}): RingStatus {
  return {
    enabled: false, ringId: null, version: null, relayUrl: null, hostedRelayUrl: "wss://r", connection: "off",
    retryIn: null, connectionError: null, limited: false, problem: null, problemDetail: null, members: [],
    quotaResetAt: null, relayHosted: true, local: "daemon", hosts: [], membership: "none", removed: null,
    oldRing: null, finishingOffline: false, busy: false, ...over,
  };
}

export const me: MemberView = { name: "desk", role: "desktop", signKey: "me", thisApp: true, thisComputer: true, hostId: null, removable: true, presence: { kind: "online" }, enrollmentId: "u1", kind: "computer", daemonKey: "mek", daemonPresence: { kind: "online" } };
export const tower: MemberView = { ...me, name: "tower", signKey: "tw", thisApp: false, thisComputer: false, enrollmentId: "u2", daemonKey: "twk", presence: { kind: "online" } };
export const phone: MemberView = { ...me, name: "Pixel", signKey: "ph", role: "mobile", kind: "mobile", thisApp: false, thisComputer: false, enrollmentId: "u3", daemonKey: null, daemonPresence: null };

export function member(over: Partial<RingStatus> = {}): RingStatus {
  return ringStatus({ enabled: true, ringId: "abcdefgh", version: 2, relayUrl: "wss://r", connection: "connected", membership: "member", members: [me, tower, phone], ...over });
}
