import { invoke } from "@tauri-apps/api/core";
import type { Tab } from "../types";
import { daemonHost } from "../hosts/localHost";
import { isUsableStatus, type RegistrySnapshot } from "../hosts/registry";
import { fmt } from "../hosts/strings";
import { isHostError } from "../hosts/types";

export const CUSTOM_TITLE_CAPABILITY = "term.custom-title";

// Convenience validation matches protocol Unicode White_Space, not JS trim's BOM rule.
export function customTitleError(text: string): string | null {
  if (/[\u0000-\u001f\u007f-\u009f\u2028\u2029]/u.test(text)) return fmt("tab.rename.controls");
  const normalized = text.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "");
  if (!normalized) return fmt("tab.rename.blank");
  return [...normalized].length > 256 ? fmt("tab.rename.tooLong") : null;
}

export function customTitleUnavailable(tab: Tab, snap: RegistrySnapshot): string | null {
  const host = daemonHost(tab);
  if (!host) return fmt("tab.rename.inProcess");
  const status = snap.status[host];
  if (!isUsableStatus(status)) return fmt("tab.rename.offline");
  if (!status?.daemonCapabilities?.includes(CUSTOM_TITLE_CAPABILITY)) return fmt("tab.rename.unsupported");
  if (!snap.live[host]?.some(t => t.terminal === tab.terminal)) return fmt("tab.rename.gone");
  return null;
}

export interface CustomTitleTarget {
  tabId: string;
  host: string;
  terminal: string;
  configGeneration: number | undefined;
  config: string;
}
interface DialogState {
  target: CustomTitleTarget;
  seed: string;
  operation: "save" | "clear" | null;
  error: string | null;
  retryClear: boolean;
}
interface State { dialog: DialogState | null; pending: boolean }
const configOf = (snap: RegistrySnapshot, host: string) => JSON.stringify(snap.configs.find(c => c.id === host) ?? null);

export function captureCustomTitleTarget(tab: Tab, snap: RegistrySnapshot): CustomTitleTarget | null {
  const host = daemonHost(tab);
  return host ? { tabId: tab.id, host, terminal: tab.terminal!, configGeneration: snap.status[host]?.configGeneration, config: configOf(snap, host) } : null;
}

export function customTitleTargetMatches(target: CustomTitleTarget, tab: Tab, snap: RegistrySnapshot): boolean {
  return tab.id === target.tabId && tab.terminal === target.terminal && daemonHost(tab) === target.host
    && snap.status[target.host]?.configGeneration === target.configGeneration && configOf(snap, target.host) === target.config;
}

// One explicit action, no outbox/retry and no Tab setter. List reconciliation alone owns
// displayed values, including an echo preceding an error or a newer writer preceding success.
export class CustomTitleController {
  private state: State = { dialog: null, pending: false };
  private listeners = new Set<() => void>();
  constructor(
    private tabs: () => Tab[],
    private snapshot: () => RegistrySnapshot,
    private send: (command: string, args: Record<string, unknown>) => Promise<unknown> = invoke,
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
    if (!tab || (snap.live[target.host] && !snap.live[target.host]!.some(t => t.terminal === target.terminal))) return undefined;
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
      await this.send("host_term_update", { host: dialog.target.host, terminal: dialog.target.terminal, meta: { customTitle: value } });
    } catch (e) {
      failure = isHostError(e) && (e.code === "indeterminate" || e.code === "timeout")
        ? fmt("tab.rename.notConfirmed")
        : fmt("tab.rename.failed", { error: isHostError(e) || e instanceof Error ? e.message : String(e) });
    }
    // Dismiss/close/reconfigure cannot resurrect a dialog. No reply ever installs its value.
    const stillCurrent = this.state.dialog === pending && this.current(dialog.target);
    this.emit({ pending: false, dialog: stillCurrent && failure ? { ...pending, operation: null, error: failure, retryClear: value === null } : null });
  };
}
