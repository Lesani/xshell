import { resolveHostAlias } from "../hosts/registry";
import { useHostsSnapshot } from "../hosts/useHosts";
import { LOCAL_HOST } from "../hosts/localHost";
import { fmt } from "../hosts/strings";
import { HostUpdateAction } from "./HostUpdateAction";

export function HostUpdateBanner({ host: id }: { host: string | undefined }) {
  const snap = useHostsSnapshot();
  if (id === LOCAL_HOST) {
    // This machine switches its own background program: no build hashes, no "check
    // Settings" sentence. While it switches, the approved progress line; otherwise nothing.
    const status = snap.status[LOCAL_HOST];
    const switching = status?.phase === "upgrading" || status?.phase === "preparing" || status?.phase === "installing";
    if (!status || !switching) return null;
    return <div className="host-update-banner"><div className="host-update-action">
      <span role="status" data-local-switching>{fmt(status.phase === "upgrading" ? "update.restarting" : "update.sending")}</span>
    </div></div>;
  }
  const host = snap.available?.find(h => h.id === id || h.aliases.includes(id ?? "")) ?? snap.configs.find(h => h.id === id);
  if (!host || !id) return null;
  return <div className="host-update-banner"><HostUpdateAction host={{ id, name: host.name }} status={snap.status[resolveHostAlias(snap, id)]} description /></div>;
}
