import type { Tab } from "../types";
import type { HostId } from "./types";

// The Local Host's Terminals run in the Daemon this Desktop starts (ADR-0005) and reach the
// frontend like a Remote Host's, under the wire id "local". A Tab or Project never carries
// it: a Local Daemon Tab has `terminal` set and `host` undefined.

export const LOCAL_HOST: HostId = "local";

// What `local_host_info` reports: whether new Local Tabs run through the Daemon.
export interface LocalHostInfo {
  mode: "daemon" | "in-process";
  reason: string | null;
}

// The Host whose Daemon serves this Tab's Terminal (wire id), or null for an in-process Tab.
export const daemonHost = (tab: Pick<Tab, "host" | "terminal">): HostId | null =>
  tab.terminal ? (tab.host ?? LOCAL_HOST) : null;

// A wire Host id as a Tab's or Project's `host`: "local" becomes undefined.
export const tabHostOf = (wire: HostId): HostId | undefined => (wire === LOCAL_HOST ? undefined : wire);

export const isLocalHost = (h: HostId | null | undefined): boolean => h === LOCAL_HOST;
