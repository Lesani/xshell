import { useSyncExternalStore } from "react";
import { exactUpdateConnected, hostUpdates, updateAction } from "../hosts/hostUpdate";
import { useHostsSnapshot } from "../hosts/useHosts";
import { fmt } from "../hosts/strings";
import { buildText } from "../buildId";
import type { HostConfig, HostStatus } from "../hosts/types";
import { ConfirmDialog } from "./ConfirmDialog";

export function HostUpdateAction({ host, status, description = false }: { host: HostConfig; status: HostStatus | undefined; description?: boolean }) {
  const state = useSyncExternalStore(hostUpdates.subscribe, hostUpdates.getSnapshot, hostUpdates.getSnapshot);
  const healthy = exactUpdateConnected(status);
  const action = updateAction(status);
  const busy = !healthy && (!!state.invoking[host.id] || status?.phase === "upgrading" || status?.phase === "preparing");
  const replace = (action ?? status?.updateAction) === "replace";
  const error = healthy ? null : state.errors[host.id] || (status?.errorHint === "update-failed" ? status.lastError : null);
  const oldOwner = !!status?.updateUnavailable && !!status.updateAction;
  const visible = action || busy || oldOwner || error;
  if (!visible) return null;
  return <div className="host-update-action">
    {oldOwner ? <span>{fmt("hosts.update.oldOwner", { host: host.name })}</span> : <>
      {description && !busy && status && <span>{fmt(replace ? "hosts.update.buildDiffers" : "hosts.update.older", { host: host.name, daemonVersion: status.daemonVersion ?? "", desktopVersion: status.desktopVersion, version: status.desktopVersion, daemonBuild: buildText(status.daemonBuild), desktopBuild: buildText(status.desktopBuild) })}</span>}
      {busy ? <span role="status">{fmt(status?.phase === "upgrading" ? (replace ? "hosts.update.switching" : "hosts.update.waiting") : "hosts.update.preparing", { host: host.name })}</span> : action && <button className="btn btn-ghost settings-action-btn" disabled={!!state.confirmation} onClick={() => status && hostUpdates.ask(host, status)}>{fmt(replace ? "hosts.update.replace" : "hosts.update.action")}</button>}
    </>}
    {error && <span className="host-row-error" role="alert">{fmt(replace ? "hosts.update.replaceFailed" : "hosts.update.failed", { host: host.name, error })}</span>}
  </div>;
}

// Mounted once in App, not inside a Tab or settings row: an open confirmation never
// changes its Host when focus/navigation changes, and cannot appear twice.
export function HostUpdateConfirmation() {
  const { confirmation } = useSyncExternalStore(hostUpdates.subscribe, hostUpdates.getSnapshot, hostUpdates.getSnapshot);
  const snap = useHostsSnapshot();
  if (!confirmation) return null;
  const { host, action } = confirmation;
  const count = snap.live[host.id]?.length;
  return <ConfirmDialog title={fmt("hosts.update.confirmTitle", { host: host.name })}
    body={fmt("hosts.update.confirmBody", { host: host.name }) + (count === undefined ? "" : ` ${fmt("hosts.row.terminals", { n: count })}`)}
    confirm={fmt(action === "replace" ? "hosts.update.replace" : "hosts.update.action")} onConfirm={hostUpdates.confirm} onCancel={hostUpdates.cancel} />;
}
