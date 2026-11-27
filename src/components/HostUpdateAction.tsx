import { useEffect, useState, useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import { exactUpdateConnected, hostUpdates, skipReason, updateAction, updateSource, type AllResult, type UpdateHost } from "../hosts/hostUpdate";
import { useHostsSnapshot } from "../hosts/useHosts";
import { fmt } from "../hosts/strings";
import type { HostStatus } from "../hosts/types";
import { ConfirmDialog } from "./ConfirmDialog";
import { CodeCopy } from "./CodeCopy";

// The command that installs this Desktop's version on a machine without a window
// (`host_install_command`), fetched once per window.
let installCommand: Promise<string> | null = null;
export function fetchInstallCommand(call: typeof invoke = invoke): Promise<string> {
  installCommand ??= call<string>("host_install_command").catch(e => { installCommand = null; throw e; });
  return installCommand;
}

function useInstallCommand(wanted: boolean): string | null {
  const [command, setCommand] = useState<string | null>(null);
  useEffect(() => {
    if (!wanted || command) return;
    let live = true;
    fetchInstallCommand().then(c => { if (live) setCommand(c); }, () => {});
    return () => { live = false; };
  }, [wanted, command]);
  return command;
}

/// A machine whose xshell cannot take an update over the Ring: how to update it there.
export function RingUpdateUnsupported({ name }: { name: string }) {
  const command = useInstallCommand(true);
  return <span className="host-update-unsupported">
    <span>{fmt("update.tooOld", { name })}</span>
    {command && <CodeCopy text={command} />}
  </span>;
}

// A failed update in approved words only: never the backend's text, a Host id or a build.
// COPY-GAP: no text for a generic update failure; "hasn't reconnected" is the nearest.
// The backend may report an outcome already in approved words (update.rolledBack, …): kept.
const APPROVED = (["update.rolledBack", "update.offline", "update.busy", "update.notBack", "update.tooOld", "update.noBuild", "update.downloadFailed", "update.verifyFailed", "update.alreadyCurrent"] as const)
  .map(k => new RegExp(`^${fmt(k).replace(/[.*+?^$()|[\]\\]/g, "\\$&").replace(/\\?\{\w+\\?\}/g, ".+")}$`));

// "This updates 1 machine…" / "This updates {n} machines…".
export function updateAllBody(n: number): string {
  return n === 1 ? fmt("update.all.body.one") : fmt("update.all.body", { n });
}

export function updateErrorText(name: string, error: string): string {
  if (APPROVED.some(re => re.test(error.trim())) && !/r_[0-9a-f]{8}/.test(error)) return error.trim();
  console.debug("update failed:", error.split(":")[0]);
  // The Daemon's failure codes when it fetches a release itself.
  if (error.includes("update-download-failed")) return fmt("update.downloadFailed", { name });
  if (error.includes("update-verify-failed")) return fmt("update.verifyFailed", { name });
  if (/offline|unreachable|not connected/i.test(error)) return fmt("update.offline", { name });
  if (/\bbusy\b/i.test(error)) return fmt("update.busy", { name });
  return fmt("update.notBack", { name });
}

export function HostUpdateAction({ host, status, description = false }: { host: UpdateHost; status: HostStatus | undefined; description?: boolean }) {
  const state = useSyncExternalStore(hostUpdates.subscribe, hostUpdates.getSnapshot, hostUpdates.getSnapshot);
  const healthy = exactUpdateConnected(status);
  const action = status?.host === host.id ? updateAction(status) : null;
  const busy = !healthy && (!!state.invoking[host.id] || status?.phase === "upgrading" || status?.phase === "preparing" || status?.phase === "installing");
  const replace = (action ?? status?.updateAction) === "replace";
  const error = healthy ? null : state.errors[host.id] || (status?.errorHint === "update-failed" ? status.lastError : null);
  const unavailable = status?.updateUnavailable && (!!status.updateAction || !!status.routeUpdateUnavailable) ? status.updateUnavailable : null;
  const route = status?.routeUpdateUnavailable;
  // A running update may no longer offer itself; then its source is the one confirmed.
  const source = updateSource(status) ?? (busy ? state.sources[host.id] : null);
  const visible = action || busy || unavailable || route || error;
  if (!visible) return null;
  return <div className="host-update-action">
    {/* Never a build hash: a different build is just "Update available". */}
    {description && !busy && status && (action || unavailable) && <span>{source === "release" ? fmt("update.release.older", { host: host.name, daemonVersion: status.daemonVersion ?? "", version: status.updateVersion ?? "" })
      : replace ? `${host.name}: ${fmt("update.available")}` : fmt("hosts.update.older", { host: host.name, daemonVersion: status.daemonVersion ?? "", desktopVersion: status.desktopVersion })}</span>}
    {route === "ring-update-unsupported" && !unavailable && <RingUpdateUnsupported name={host.name} />}
    {route === "identity-bound-alias" && <span>This identity-bound alias cannot update the Daemon. Use its configured SSH route.</span>}
    {unavailable ? <span>{fmt(unavailable === "desktop-owned" ? "hosts.update.desktopOwned" : "hosts.update.oldOwner", { host: host.name })}</span> : <>
      {/* With a description (the bar, Hosts settings) the progress line names its machine: the bar may list several. */}
      {busy ? <span role="status">{description && `${host.name}: `}{fmt(status?.phase === "upgrading" ? "update.restarting" : source === "release" ? "update.downloading" : "update.sending")}</span>
        : action && <button className="btn btn-ghost settings-action-btn" disabled={!!state.confirmation || !!(state.all && !state.all.done)} onClick={() => status && hostUpdates.ask(host, status)}>{fmt(error ? "update.retry" : replace ? "hosts.update.replace" : "update.action")}</button>}
    </>}
    {error && <span className="host-row-error" role="alert">{updateErrorText(host.name, error)}</span>}
  </div>;
}

/// One line of an "Update all" result.
function resultText(r: AllResult): string {
  switch (r.outcome) {
    case "ok": return `${r.host.name}: ${fmt("update.done", { new: r.version })}`;
    case "failed": return updateErrorText(r.host.name, r.error ?? "");
    case "skipped":
      switch (r.reason) {
        case "current": return `${r.host.name}: ${fmt("update.current")}`;
        case "offline": return fmt("update.offline", { name: r.host.name });
        case "unsupported": return fmt("update.tooOld", { name: r.host.name });
        case "busy": return fmt("update.busy", { name: r.host.name });
      }
  }
}

const count = (n: number) => n === 1 ? fmt("update.count.one") : fmt("update.count.other", { n });

/// "Update all": updates every machine in `hosts` that has an update offered, one after
/// another, after one confirmation. Self-contained: statuses come from the Host registry.
export function HostUpdateAll({ hosts }: { hosts: UpdateHost[] }) {
  const snap = useHostsSnapshot();
  const state = useSyncExternalStore(hostUpdates.subscribe, hostUpdates.getSnapshot, hostUpdates.getSnapshot);
  const statusOf = (id: string) => snap.status[id];
  const eligible = hosts.filter(h => statusOf(h.id)?.host === h.id && !skipReason(statusOf(h.id)));
  const run = state.all;
  if (run && run.confirmed) {
    return <div className="host-update-all" role="status">
      {run.current && <span>{run.current.name}: {fmt(state.sources[run.current.id] === "release" ? "update.downloading" : "update.sending")}</span>}
      {[...run.results, ...(run.done ? run.skipped : [])].map(r => <span key={r.host.id} className={r.outcome === "failed" ? "host-row-error" : undefined}>{resultText(r)}</span>)}
    </div>;
  }
  if (eligible.length < 1) return null;
  return <div className="host-update-all">
    <span>{count(eligible.length)}</span>
    <button className="btn btn-ghost settings-action-btn" disabled={!!state.confirmation || !!run}
      onClick={() => hostUpdates.askAll(hosts.map(h => ({ host: h, status: statusOf(h.id) })))}>{fmt("update.all", { n: eligible.length })}</button>
  </div>;
}

// Mounted once in App, not inside a Tab or settings row: an open confirmation never
// changes its Host when focus/navigation changes, and cannot appear twice.
export function HostUpdateConfirmation() {
  const { confirmation, all } = useSyncExternalStore(hostUpdates.subscribe, hostUpdates.getSnapshot, hostUpdates.getSnapshot);
  const snap = useHostsSnapshot();
  if (all && !all.confirmed) {
    // The list of machines stays visible beside the approved body.
    const names = all.queue.map(c => c.host.name).join(", ");
    return <ConfirmDialog title={fmt("update.all.title")}
      body={`${updateAllBody(all.queue.length)} ${names}`}
      confirm={fmt("update.all.confirm")} cancel={fmt("update.all.cancel")} onConfirm={hostUpdates.confirmAll} onCancel={hostUpdates.cancelAll} />;
  }
  if (!confirmation) return null;
  const { host, action } = confirmation;
  const n = snap.live[host.id]?.length;
  return <ConfirmDialog title={fmt("hosts.update.confirmTitle", { host: host.name })}
    body={fmt("hosts.update.confirmBody", { host: host.name }) + (n === undefined ? "" : ` ${fmt("hosts.row.terminals", { n })}`)}
    confirm={fmt(action === "replace" ? "hosts.update.replace" : "update.action")} onConfirm={hostUpdates.confirm} onCancel={hostUpdates.cancel} />;
}
