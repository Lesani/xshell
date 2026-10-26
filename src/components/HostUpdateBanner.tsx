import { useHostsSnapshot } from "../hosts/useHosts";
import { HostUpdateAction } from "./HostUpdateAction";

export function HostUpdateBanner({ host: id }: { host: string | undefined }) {
  const snap = useHostsSnapshot();
  const host = snap.configs.find(h => h.id === id);
  if (!host) return null;
  return <div className="host-update-banner"><HostUpdateAction host={host} status={snap.status[host.id]} description /></div>;
}
