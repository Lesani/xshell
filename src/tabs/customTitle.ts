import { invoke } from "@tauri-apps/api/core";
import type { Tab } from "../types";
import { daemonHost } from "../hosts/localHost";
import { isUsableStatus, resolveHostAlias, type RegistrySnapshot } from "../hosts/registry";
import { fmt } from "../hosts/strings";
import { isHostError } from "../hosts/types";
import { customTitleError, normalizeCustomTitle } from "./customTitleValidation";
export { customTitleError, normalizeCustomTitle } from "./customTitleValidation";

export const CUSTOM_TITLE_CAPABILITY = "term.custom-title";

export function customTitleUnavailable(tab: Tab, snap: RegistrySnapshot): string | null {
  if (isInProcessTitleTab(tab)) return null;
  const host = daemonHost(tab);
  if (tab.type !== "terminal" || !host) return fmt("tab.rename.gone");
  const status = snap.status[resolveHostAlias(snap, host)];
  if (!isUsableStatus(status)) return fmt("tab.rename.offline");
  if (!status?.daemonCapabilities?.includes(CUSTOM_TITLE_CAPABILITY)) return fmt("tab.rename.unsupported");
  if (!snap.live[resolveHostAlias(snap, host)]?.some(t => t.terminal === tab.terminal)) return fmt("tab.rename.gone");
  return null;
}

export const isInProcessTitleTab = (tab: Tab): boolean => tab.type === "terminal" && !tab.host && !tab.terminal;
export interface InProcessTitleTarget {
  kind: "in-process";
  tabId: string;
  createdAt: Tab["createdAt"];
  projectPath: Tab["projectPath"];
  agent: Tab["agent"];
  shellMode: Tab["shellMode"];
  shellId: Tab["shellId"];
}
export interface DaemonTitleTarget {
  kind: "daemon";
  tabId: string;
  host: string;
  terminal: string;
  configGeneration: number | undefined;
  config: string;
}
export type CustomTitleTarget = InProcessTitleTarget | DaemonTitleTarget;
export type SaveInProcessTitle = (target: InProcessTitleTarget, value: string | null) => Promise<void>;
export function inProcessTitleTargetMatches(target: InProcessTitleTarget, tab: Tab): boolean {
  return isInProcessTitleTab(tab) && tab.id === target.tabId && tab.createdAt === target.createdAt
    && tab.projectPath === target.projectPath && tab.agent === target.agent
    && tab.shellMode === target.shellMode && tab.shellId === target.shellId;
}
interface DialogState {
  target: CustomTitleTarget;
  seed: string;
  operation: "save" | "clear" | null;
  error: string | null;
  retryClear: boolean;
}
interface State { dialog: DialogState | null; pending: boolean }
const configOf = (snap: RegistrySnapshot, host: string) => JSON.stringify([snap.configs.find(c => c.id === host) ?? null, snap.available?.find(h => h.id === host || h.aliases.includes(host))?.daemonKey ?? null]);

export function captureCustomTitleTarget(tab: Tab, snap: RegistrySnapshot): CustomTitleTarget | null {
  if (isInProcessTitleTab(tab)) return { kind: "in-process", tabId: tab.id, createdAt: tab.createdAt,
    projectPath: tab.projectPath, agent: tab.agent, shellMode: tab.shellMode, shellId: tab.shellId };
  const host = daemonHost(tab);
  return tab.type === "terminal" && host ? { kind: "daemon", tabId: tab.id, host, terminal: tab.terminal!, configGeneration: snap.status[resolveHostAlias(snap, host)]?.configGeneration, config: configOf(snap, host) } : null;
}

export function customTitleTargetMatches(target: CustomTitleTarget, tab: Tab, snap: RegistrySnapshot): boolean {
  if (target.kind === "in-process") return inProcessTitleTargetMatches(target, tab);
  return tab.id === target.tabId && tab.terminal === target.terminal && daemonHost(tab) === target.host
    && snap.status[resolveHostAlias(snap, target.host)]?.configGeneration === target.configGeneration && configOf(snap, target.host) === target.config;
}

// One explicit action, no outbox or automatic retry. Daemon replies never install values;
// only the local persistence boundary may patch Desktop-owned in-process names.
export class CustomTitleController {
  private state: State = { dialog: null, pending: false };
  private listeners = new Set<() => void>();
  constructor(
    private tabs: () => Tab[],
    private snapshot: () => RegistrySnapshot,
    private send: (command: string, args: Record<string, unknown>) => Promise<unknown> = invoke,
    private saveLocal: SaveInProcessTitle = async () => { throw new Error("Local name saving is unavailable"); },
  ) {}
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  getSnapshot = () => this.state;
  private emit(state: State) { this.state = state; for (const l of this.listeners) l(); }

  open(tab: Tab, captured?: CustomTitleTarget | null): boolean {
    if (this.state.pending) return false;
    const snap = this.snapshot();
    const target = captured ?? captureCustomTitleTarget(tab, snap);
    if (!target || !customTitleTargetMatches(target, tab, snap) || customTitleUnavailable(tab, snap)) return false;
    this.emit({ pending: false, dialog: {
      target,
      seed: tab.customTitle || tab.title, operation: null, error: null, retryClear: false,
    } });
    return true;
  }

  private current(target: CustomTitleTarget): Tab | undefined {
    const snap = this.snapshot();
    const tab = this.tabs().find(t => customTitleTargetMatches(target, t, snap));
    if (!tab || (target.kind === "daemon" && snap.live[resolveHostAlias(snap, target.host)] && !snap.live[resolveHostAlias(snap, target.host)]!.some(t => t.terminal === target.terminal))) return undefined;
    return tab;
  }

  invalidate = () => {
    if (this.state.dialog && !this.current(this.state.dialog.target)) this.dismiss();
  };
  dismiss = () => { this.emit({ ...this.state, dialog: null }); };

  submit = async (value: string | null) => {
    const dialog = this.state.dialog;
    if (!dialog || this.state.pending) return;
    const tab = this.current(dialog.target);
    if (!tab) { this.dismiss(); return; }
    const error = customTitleUnavailable(tab, this.snapshot()) || (value === null ? null : customTitleError(value));
    if (error) { this.emit({ ...this.state, dialog: { ...dialog, error } }); return; }
    const pending: DialogState = { ...dialog, operation: value === null ? "clear" : "save", error: null };
    this.emit({ pending: true, dialog: pending });
    let failure: string | null = null;
    try {
      const normalized = value === null ? null : normalizeCustomTitle(value);
      if (dialog.target.kind === "in-process") await this.saveLocal(dialog.target, normalized);
      else await this.send("host_term_update", { host: dialog.target.host, terminal: dialog.target.terminal, meta: { customTitle: normalized } });
    } catch (e) {
      failure = dialog.target.kind === "in-process" ? fmt("tab.rename.localFailed", { error: e instanceof Error ? e.message : String(e) }) : isHostError(e) && (e.code === "indeterminate" || e.code === "timeout")
        ? fmt("tab.rename.notConfirmed")
        : fmt("tab.rename.failed", { error: isHostError(e) || e instanceof Error ? e.message : String(e) });
    }
    // Dismiss/close/reconfigure cannot resurrect a dialog. No reply ever installs its value.
    const stillCurrent = this.state.dialog === pending && this.current(dialog.target);
    this.emit({ pending: false, dialog: stillCurrent && failure ? { ...pending, operation: null, error: failure, retryClear: value === null } : null });
  };
}
