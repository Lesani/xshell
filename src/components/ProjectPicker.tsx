import { useEffect, useMemo, useRef, useState } from "react";
import { buildProjectRows, loadPickerProjects, type AgentProjectLists, type PickerRow } from "../hosts/hostProjects";
import { isPinned, toProjectKey, type ProjectKey } from "../hosts/projectKey";
import { isUsableStatus } from "../hosts/registry";
import { statusLabel, useHostsSnapshot } from "../hosts/useHosts";
import { fmt } from "../hosts/strings";
import type { HostId } from "../hosts/types";
import { HostBadge } from "./HostBadge";
import { Check, FolderPlus, Search } from "lucide-react";
import type { ProjectInfo } from "../types";
import { AGENT_IDS, AGENTS, AgentIcon } from "../agents";
import { useTooltip, ttProps } from "./Tooltip";

interface ProjectPickerProps {
  // App merges agents here; never use this prop as raw Claude discovery.
  allProjects: ProjectInfo[];
  savedPaths: ProjectKey[];
  onToggle: (key: ProjectKey) => void;
  onBrowse: () => void;
  onClose: () => void;
  onRefresh?: () => void;
  // Pin a folder on a Remote Host by its path (validated on the Host).
  onAddRemotePath?: (host: HostId, path: string) => Promise<"ok" | "notFound" | "offline">;
}

// Raw per-agent lists are fetched here, independently of App's merged discovery.
interface HostLists { lists: AgentProjectLists; stale: boolean; loaded: boolean }

export function ProjectPicker({ savedPaths, onToggle, onBrowse, onClose, onRefresh, onAddRemotePath }: ProjectPickerProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [localLists, setLocalLists] = useState<AgentProjectLists>({});
  const [filter, setFilter] = useState("");
  const { tt, Tooltip } = useTooltip();
  const hostsSnap = useHostsSnapshot();
  const hosts = hostsSnap.configs;
  const [hostLists, setHostLists] = useState<Record<HostId, HostLists>>({});

  // Refresh every agent's list when the dialog opens so directories used since app start
  // (or since the last open) appear without requiring a restart.
  useEffect(() => {
    onRefresh?.();
    let alive = true;
    loadPickerProjects().then(r => { if (alive) setLocalLists(prev => ({ ...prev, ...r.lists })); });
    return () => { alive = false; };
  }, []);

  // Each configured Host's lists — live when connected, else the cached copy marked stale.
  const usableKey = hosts.filter(h => isUsableStatus(hostsSnap.status[h.id])).map(h => h.id).join(",");
  useEffect(() => {
    let alive = true;
    for (const h of hosts) {
      loadPickerProjects(h.id).then(r => {
        if (!alive) return;
        setHostLists(prev => ({ ...prev, [h.id]: {
          lists: { ...prev[h.id]?.lists, ...r.lists },
          stale: r.stale || (!r.loaded && !!prev[h.id]?.loaded),
          loaded: r.loaded || !!prev[h.id]?.loaded,
        } }));
      });
    }
    return () => { alive = false; };
  }, [hosts, usableKey]);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => { document.removeEventListener("mousedown", handleClick); document.removeEventListener("keydown", handleKey); };
  }, [onClose]);

  const rows = useMemo(() => buildProjectRows(localLists, filter), [localLists, filter]);
  const hostRows = useMemo(() => Object.fromEntries(hosts.map(h => {
    const l = hostLists[h.id];
    return [h.id, l ? buildProjectRows(l.lists, filter) : []];
  })) as Record<HostId, PickerRow[]>, [hosts, hostLists, filter]);
  const totalRows = rows.length + Object.values(hostRows).reduce((n, r) => n + r.length, 0);

  // Checked state compares with today's normalizePath within the row's Host (amendment 25).
  const isChecked = (path: string, host?: HostId) => isPinned(savedPaths, host, path);

  const renderRow = (row: PickerRow, host?: HostId) => (
    <div key={`${host ?? "local"}:${row.path}`} className={`picker-item ${isChecked(row.path, host) ? "checked" : ""}`} onClick={() => onToggle(toProjectKey(host, row.path))}>
      <div className="picker-check">
        {isChecked(row.path, host) && <Check size={12} />}
      </div>
      <div className="picker-item-info">
        <div className="picker-item-name">{row.name}</div>
        <div className="picker-item-path" {...ttProps(tt, row.path)}>{row.path}</div>
      </div>
      <div className="picker-item-agents">
        {AGENT_IDS.filter(id => row.counts[id]).map(id => (
          <span key={id} className="picker-agent" {...ttProps(tt, `${row.counts[id]} ${AGENTS[id].label} session${row.counts[id] === 1 ? "" : "s"}`)}><AgentIcon agent={id} size={12} /></span>
        ))}
        <span className="picker-item-count" {...ttProps(tt, "Total sessions in this directory")}>{AGENT_IDS.reduce((sum, id) => sum + (row.counts[id] ?? 0), 0)}</span>
      </div>
    </div>
  );

  const hasHosts = hosts.length > 0;

  return (
    <div className="picker-overlay">
      <div className="picker" ref={ref}>
        <div className="picker-band">
          <span className="picker-band-label">Add Projects</span>
        </div>

        <div className="picker-head">
          <div className="picker-title">Pin projects to your sidebar</div>
          <div className="picker-sub">{hasHosts ? fmt("picker.sub") : "Every directory where a coding agent has been used on this machine. The marks on the right show which agent has sessions there."}</div>
        </div>

        <div className="picker-search">
          <Search size={12} />
          <input autoFocus value={filter} onChange={(e) => setFilter(e.target.value)} placeholder={`Filter ${totalRows || ""} directories…`} spellCheck={false} />
        </div>

        <div className="picker-listwrap">
          <span className="corner-dot corner-dot-tl" aria-hidden />
          <span className="corner-dot corner-dot-tr" aria-hidden />
          <span className="corner-dot corner-dot-bl" aria-hidden />
          <span className="corner-dot corner-dot-br" aria-hidden />
          <div className="picker-list">
            {hasHosts && <div className="picker-group-head"><span className="picker-group-name">{fmt("picker.group.local")}</span></div>}
            {rows.map(row => renderRow(row))}
            {rows.length === 0 && (
              <div className="picker-empty">{filter.trim() ? "No directories match your filter." : "No coding agent sessions found on this machine."}</div>
            )}
            {hosts.map(h => {
              const st = hostsSnap.status[h.id];
              const l = hostLists[h.id];
              const hr = hostRows[h.id] ?? [];
              const neverConnected = !l?.loaded && hostsSnap.live[h.id] == null;
              return (
                <div key={h.id} className="picker-group">
                  <div className="picker-group-head">
                    <HostBadge host={h.id} size="md" />
                    <span className="picker-group-name">{h.name}</span>
                    <span className={`picker-group-status ${isUsableStatus(st) ? "ok" : "down"}`}>{fmt("picker.group.hostStatus", { status: statusLabel(st) })}</span>
                    {l?.stale && <span className="picker-group-stale">{fmt("picker.group.stale")}</span>}
                  </div>
                  {hr.map(row => renderRow(row, h.id))}
                  {hr.length === 0 && <div className="picker-empty">{neverConnected ? fmt("picker.group.neverConnected") : fmt("picker.group.empty")}</div>}
                  {onAddRemotePath && <AddRemotePath host={h.id} usable={isUsableStatus(st)} onAdd={onAddRemotePath} />}
                </div>
              );
            })}
          </div>
        </div>

        <div className="picker-footer">
          <span className="picker-footer-hint">Missing one? Pin any folder manually.</span>
          <button className="btn" onClick={onBrowse}><FolderPlus size={12} /> Browse…</button>
          <button className="btn btn-primary" onClick={onClose}>Done</button>
        </div>
      </div>
      {Tooltip}
    </div>
  );
}

// Per-Host "add a folder by path" — the Browse dialog only sees this computer.
function AddRemotePath({ host, usable, onAdd }: { host: HostId; usable: boolean; onAdd: (host: HostId, path: string) => Promise<"ok" | "notFound" | "offline"> }) {
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async () => {
    const p = path.trim();
    if (!p || busy) return;
    if (!usable) { setError(fmt("picker.addPath.offline")); return; }
    setBusy(true);
    const r = await onAdd(host, p).catch(() => "notFound" as const);
    setBusy(false);
    if (r === "ok") { setPath(""); setError(null); }
    else setError(fmt(r === "offline" ? "picker.addPath.offline" : "picker.addPath.notFound"));
  };
  return (
    <div className="picker-addpath">
      <input value={path} onChange={(e) => { setPath(e.target.value); setError(null); }} onKeyDown={(e) => { if (e.key === "Enter") submit(); }} placeholder={fmt("picker.addPath.placeholder")} spellCheck={false} />
      <button className="btn" disabled={!path.trim() || busy} onClick={submit}><FolderPlus size={12} /> {fmt("picker.addPath.button")}</button>
      {error && <div className="picker-addpath-error">{error}</div>}
    </div>
  );
}
