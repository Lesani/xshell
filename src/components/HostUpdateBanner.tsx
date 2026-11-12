import { resolveHostAlias } from "../hosts/registry";
import { useHostsSnapshot } from "../hosts/useHosts";
import { LOCAL_HOST } from "../hosts/localHost";
import { fmt } from "../hosts/strings";
import { buildText } from "../buildId";
import { HostUpdateAction } from "./HostUpdateAction";

export function HostUpdateBanner({ host: id }: { host: string | undefined }) {
  const snap = useHostsSnapshot();
  if (id === LOCAL_HOST) {
    const status = snap.status[LOCAL_HOST];
    if (!status?.daemonVersion || status.phase !== null || (status.status !== "upgrade-pending" && status.status !== "build-differs")) return null;
    // Native classification drives this notice; Local operations stay in Settings.
    return <div className="host-update-banner"><div className="host-update-action">
      <span>{fmt(status.status === "build-differs" ? "hosts.update.buildDiffers" : "hosts.update.older", { host: fmt("local.section.title"), daemonVersion: status.daemonVersion, desktopVersion: status.desktopVersion, version: status.desktopVersion, daemonBuild: buildText(status.daemonBuild), desktopBuild: buildText(status.desktopBuild) })}</span>
      <span>{fmt("local.daemon.settingsHint")}</span>
    </div></div>;
  }
  const host = snap.available?.find(h => h.id === id || h.aliases.includes(id ?? "")) ?? snap.configs.find(h => h.id === id);
  if (!host || !id) return null;
  return <div className="host-update-banner"><HostUpdateAction host={{ id, name: host.name }} status={snap.status[resolveHostAlias(snap, id)]} description /></div>;
}
