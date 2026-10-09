import { useHostsSnapshot } from "../hosts/useHosts";
import { isUsableStatus } from "../hosts/registry";
import type { HostId } from "../hosts/types";
import type { TtFns } from "./Tooltip";

// Small Host marker: the Host's color dot (or its initial) — the only visible difference
// between a remote and a local Project/Tab. `tooltip` is the full hover text.
export function HostBadge({ host, tooltip, tt, size = "sm", className = "" }: { host: HostId; tooltip?: string; tt?: TtFns | null; size?: "sm" | "md"; className?: string }) {
  const snap = useHostsSnapshot();
  const cfg = snap.configs.find(c => c.id === host);
  const name = cfg?.name || host;
  const stale = !isUsableStatus(snap.status[host]);
  const hover = tooltip && tt ? { onMouseEnter: (e: React.MouseEvent<HTMLElement>) => tt.showTt(tooltip, e.currentTarget), onMouseLeave: () => tt.hideTt() } : {};
  return (
    <span className={`host-badge host-badge-${size} ${stale ? "host-badge-stale" : ""} ${className}`} style={cfg?.color ? { background: cfg.color } : undefined} {...hover} aria-label={tooltip || name}>
      {cfg?.color ? null : name.slice(0, 1).toUpperCase()}
    </span>
  );
}
