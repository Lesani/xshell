import { expect, it } from "vitest";
import type { Tab } from "../types";
import type { RegistrySnapshot } from "../hosts/registry";
import type { HostStatus, TerminalInfo } from "../hosts/types";
import { runtimeProjectKey } from "../hosts/projectRuntime";
import { sessionKeyOfTab } from "../hosts/projectKey";
import { projectTerminals } from "./projectTerminals";
const status = (state = "connected") => ({ status: state } as HostStatus);
const info = (terminal: string, fields: Partial<TerminalInfo> = {}): TerminalInfo => ({ terminal, spec: { cwd: "/p" }, meta: {}, createdAtMs: 0, pid: 1, exitCode: null, agentStatus: "working", ...fields });
const tab = (id: string, fields: Partial<Tab> = {}): Tab => ({ id, title: id, type: "terminal", projectPath: "/p", ...fields });
const hosts: RegistrySnapshot = { configs: [], status: { local: status(), h_a: status(), h_off: status("offline") }, live: { local: [info("local")], h_a: [info("agent"), info("ended", { exitCode: 0 })], h_off: [info("offline")] }, epochs: {}, agents: {} };
it("projects unlinked agents and raw Shells without history, effective custom titles and split provenance", () => {
  const tabs = [tab("agent", { terminal: "agent", host: "h_a", customTitle: "Named", groupId: "g" }), tab("shell", { shellMode: "raw", title: "Bash", projectName: "Project" }), tab("local", { terminal: "local" })];
  const rows = projectTerminals(tabs, [{ id: "g", name: "Split", layout: { kind: "leaf", tabId: "agent" } }], hosts, new Set());
  expect(rows.map(r => r.state)).toEqual(["live", "status-unavailable", "live"]);
  expect(rows.map(r => r.sessionKey)).toEqual([null, null, null]);
  expect(rows[0]).toMatchObject({ title: "Named", host: "h_a", groupName: "Split", agentStatus: "working" });
  expect(rows[1]).toMatchObject({ title: "Project", detail: "Bash", kind: "shell" });
  expect(rows[0].terminalKey).not.toBe(rows[2].terminalKey);
});
it("uses Host-qualified and case-correct runtime Project membership, preserving Pi scoped session identity and duplicate associations", () => {
  const tabs = [tab("a", { sessionId: "s", agent: "pi" }), tab("b", { sessionId: "s", agent: "pi", projectPath: "/P" }), tab("c", { sessionId: "s", host: "h_a" }), tab("d", { sessionId: "s", agent: "pi" })];
  const rows = projectTerminals(tabs, [], hosts, new Set());
  expect(new Set(rows.map(r => r.projectKey)).size).toBe(3);
  expect(rows[0].projectKey).toBe(runtimeProjectKey({ path: "/p" }));
  expect(rows.map(r => r.sessionKey)).toEqual(tabs.map(sessionKeyOfTab));
  expect(rows[0].sessionKey).toBe(rows[3].sessionKey);
});
it("requires usable authoritative membership for live; offline, ended, closing and pending-unlisted are truthful", () => {
  const tabs = [tab("offline", { host: "h_off", terminal: "offline" }), tab("ended", { host: "h_a", terminal: "ended" }), tab("closing", { host: "h_a", terminal: "agent" }), tab("pending", { host: "h_a", terminal: "pending" }), tab("never", { host: "unknown", terminal: "never" })];
  const rows = projectTerminals(tabs, [], hosts, new Set(["closing"]));
  expect(rows.map(r => r.state)).toEqual(["last-known", "ended", "closing", "unconfirmed", "last-known"]);
  expect(rows.every(r => r.agentStatus === null)).toBe(true);
  expect(projectTerminals([tab("no-project", { projectPath: undefined }), { id: "home", type: "home", title: "home" }], [], hosts, new Set())).toEqual([]);
});

it("legacy in-process Tabs are open inventory, not proof of a running process; only existing local Agent Status establishes ended", () => {
  const tabs = [tab("shell", { shellMode: "raw" }), tab("agent", { agent: "claude" }), tab("ended", { agent: "pi" })];
  const rows = projectTerminals(tabs, [], hosts, new Set(), new Map([["agent", "working"], ["ended", "ended"]]));
  expect(rows.map(row => row.state)).toEqual(["status-unavailable", "status-unavailable", "ended"]);
  expect(rows.every(row => row.agentStatus === null)).toBe(true);
});
