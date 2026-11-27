import { resolveHostAlias } from "../hosts/registry";
import { remoteHosts, useHostsSnapshot } from "../hosts/useHosts";
import { LOCAL_HOST } from "../hosts/localHost";
import { fmt } from "../hosts/strings";
import { HostUpdateAction } from "./HostUpdateAction";

// The bar above the work area. It offers the update of every machine this Desktop is
// connected to that runs another xshell, wherever the user is (Settings has its own rows).
// `host` is the machine of the focused Terminal or selected Project: it is listed first.
// Each row acts on the machine's own row id, exactly as its Settings row does; a Tab's
// alias of that machine never names the update target.
export function HostUpdateBanner({ host: id, hidden = false }: { host: string | undefined; hidden?: boolean }) {
  const snap = useHostsSnapshot();
  if (hidden) return null;
  // This machine switches its own background program: no build hashes, no "check
  // Settings" sentence. While it switches, the approved progress line; otherwise nothing.
  const local = id === LOCAL_HOST ? snap.status[LOCAL_HOST] : undefined;
  const switching = local?.phase === "upgrading" || local?.phase === "preparing" || local?.phase === "installing";
  const focused = id && id !== LOCAL_HOST ? resolveHostAlias(snap, id) : undefined;
  const hosts = remoteHosts(snap);
  const rows = [...hosts.filter(h => h.id === focused), ...hosts.filter(h => h.id !== focused)];
  if (!switching && rows.length === 0) return null;
  return <div className="host-update-banner">
    {switching && <div className="host-update-action">
      <span role="status" data-local-switching>{fmt(local!.phase === "upgrading" ? "update.restarting" : "update.sending")}</span>
    </div>}
    {rows.map(h => <HostUpdateAction key={h.id} host={{ id: h.id, name: h.name }} status={snap.status[h.id]} description />)}
  </div>;
}
