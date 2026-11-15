import type { HostStatus } from "../hosts/types";

// The seam for "Update available" / "Update all" over the Ring (slice S4). S4 exports from
// src/components/HostUpdate*:
//   - `HostUpdateAction`, props `{ host: { id: string; name: string }, status: HostStatus | undefined }`,
//     one per machine row other than this machine;
//   - `HostUpdateAll`, no props, once above the device list.
// When they land, replace the two bodies below with
//   `return <HostUpdateAction host={host} status={status} />;` and `return <HostUpdateAll />;`
// (and import both). Until then nothing is rendered.

export function MachineUpdateSlot(_props: { host: { id: string; name: string }; status: HostStatus | undefined }) {
  return null;
}

export function UpdateAllSlot() {
  return null;
}
