import type { HostStatus } from "../hosts/types";
import { HostUpdateAction, HostUpdateAll } from "../components/HostUpdateAction";

// The seam between the Ring member list and "Update" / "Update all" over the Ring.

export function MachineUpdateSlot({ host, status }: { host: { id: string; name: string }; status: HostStatus | undefined }) {
  return <HostUpdateAction host={host} status={status} />;
}

export function UpdateAllSlot({ hosts }: { hosts: { id: string; name: string }[] }) {
  return hosts.length > 0 ? <HostUpdateAll hosts={hosts} /> : null;
}
