import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { Plus, Folder as FolderIcon } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { useProjectImage } from "../hooks/useProjectImage";
import { FolderEditorDialog } from "./FolderEditorDialog";
import { RateLimitIndicator } from "./RateLimitIndicator";
import { HostBadge } from "./HostBadge";
import { useHostsSnapshot, statusLabel } from "../hosts/useHosts";
import { isUsableStatus } from "../hosts/registry";
import { fmt } from "../hosts/strings";
import logo from "../assets/logo.png";
import type { ProjectInfo, ProjectSettings, SidebarItem, SidebarFolder } from "../types";
import { asProjectKey, keyBasename, keyOf, lookupKey, parseProjectKey, sameKey, type ProjectKey } from "../hosts/projectKey";

interface SidebarProps {
  projects: ProjectInfo[];
  projectIcons: Record<string, ProjectSettings>;
  selectedProject: ProjectInfo | null;
  activeCountByProject: Map<string, number>; // keyed by lookupKey(ProjectKey)
  sidebarLayout: SidebarItem[];
  onLayoutChange: (next: SidebarItem[]) => void;
  onSelectProject: (project: ProjectInfo) => void;
  onGoHome: () => void;
  onRemoveProject: (key: ProjectKey) => void;
  onEditProject: (key: ProjectKey) => void;
  onHoverProject: (key: ProjectKey | null) => void;
  onOpenSettings: () => void;
  onAddProject: () => void;
  onCollapse: () => void;
  activeTabId: string;
  linkedProjectKey: ProjectKey | null;
  showRateLimit: boolean;
  showRateLimitCodex: boolean;
  updateAvailable: boolean;
}

function getInitials(name: string): string {
  const parts = name.replace(/[^a-zA-Z0-9\s\-_.]/g, "").split(/[\s\-_.]+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return name.slice(0, 2).toUpperCase();
}

function ProjectSidebarIcon({ iconValue, color, name, highlighted, mini }: { iconValue?: string; color?: string; name: string; highlighted: boolean; mini?: boolean }) {
  const imgSrc = useProjectImage(iconValue);
  if (imgSrc) return <div className={`ds-icon ds-project ${mini ? "ds-icon-mini" : ""}`}><img src={imgSrc} className="ds-project-img" alt="" /></div>;
  // Custom color wins — selection uses terracotta only when the project has no custom color set.
  const bg = color || (highlighted ? "var(--accent-terracotta)" : undefined);
  return <div className={`ds-icon ds-project ${mini ? "ds-icon-mini" : ""}`} style={{ background: bg }}>{iconValue || getInitials(name)}</div>;
}

// Context menu for a project (mirrors the original).
interface ProjectCtx { kind: "project"; x: number; y: number; project: ProjectInfo; }
interface FolderCtx { kind: "folder"; x: number; y: number; folder: SidebarFolder; }
type CtxState = ProjectCtx | FolderCtx | null;

function ProjectContextMenu({ ctx, onEdit, onReveal, onRemove, onClose }: { ctx: ProjectCtx; onEdit: () => void; onReveal: (() => void) | null; onRemove: () => void; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const handle = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) onClose(); };
    document.addEventListener("mousedown", handle);
    return () => document.removeEventListener("mousedown", handle);
  }, [onClose]);
  return (
    <div className="ctx-menu" ref={ref} style={{ top: ctx.y, left: ctx.x }}>
      <div className="ctx-item" onClick={() => { onEdit(); onClose(); }}>Edit project...</div>
      {onReveal && <div className="ctx-item" onClick={() => { onReveal(); onClose(); }}>Reveal in Explorer</div>}
      <div className="ctx-separator" />
      <div className="ctx-item ctx-danger" onClick={() => { onRemove(); onClose(); }}>Remove from sidebar</div>
    </div>
  );
}

function FolderContextMenu({ ctx, onRename, onUngroup, onClose }: { ctx: FolderCtx; onRename: () => void; onUngroup: () => void; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const handle = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) onClose(); };
    document.addEventListener("mousedown", handle);
    return () => document.removeEventListener("mousedown", handle);
  }, [onClose]);
  return (
    <div className="ctx-menu" ref={ref} style={{ top: ctx.y, left: ctx.x }}>
      <div className="ctx-item" onClick={() => { onRename(); onClose(); }}>Edit folder...</div>
      <div className="ctx-separator" />
      <div className="ctx-item ctx-danger" onClick={() => { onUngroup(); onClose(); }}>Delete folder (keep projects)</div>
    </div>
  );
}

// Fixed-position tooltip rendered outside scroll containers
function Tooltip({ text, rect }: { text: string; rect: DOMRect }) {
  return <div className="ds-tooltip-fixed" style={{ top: rect.top + rect.height / 2, left: rect.right + 12 }}>{text}</div>;
}

// Mini-grid shown on a collapsed folder: up to 4 contained project icons in a 2x2 layout.
function FolderMiniGrid({ paths, projectIcons, projects }: { paths: ProjectKey[]; projectIcons: Record<string, ProjectSettings>; projects: ProjectInfo[] }) {
  const byPath = useMemo(() => {
    const m = new Map<string, ProjectInfo>();
    for (const p of projects) m.set(lookupKey(keyOf(p)), p);
    return m;
  }, [projects]);
  const first4 = paths.slice(0, 4);
  return (
    <div className="ds-folder-grid">
      {first4.map(p => {
        const pl = lookupKey(p);
        const settings = projectIcons[pl];
        const proj = byPath.get(pl);
        const raw = parseProjectKey(p).path;
        const displayName = settings?.customName || proj?.name || raw.split(/[\\/]/).pop() || raw;
        return <ProjectSidebarIcon key={p} iconValue={settings?.icon} color={settings?.color} name={displayName} highlighted={false} mini />;
      })}
      {Array.from({ length: Math.max(0, 4 - first4.length) }).map((_, i) => (
        <div key={`pad-${i}`} className="ds-icon-mini ds-icon-mini-empty" />
      ))}
    </div>
  );
}

// ── Drag state ──────────────────────────────────────────────────────
// One source path, one computed target — target describes where the drop would land.
type DropTarget =
  | { kind: "merge-with-project"; path: ProjectKey }           // dropping on another top-level project → create folder
  | { kind: "merge-with-folder"; folderId: string }        // dropping on a folder (header or its body)
  | { kind: "reorder-top"; index: number }                 // insert at this top-level index (0..layout.length)
  | { kind: "reorder-in-folder"; folderId: string; index: number } // insert into a folder at this slot
  | null;

type DragSource =
  | { kind: "project"; path: ProjectKey; originFolderId: string | null }
  | { kind: "folder"; id: string };

// Only fields that matter for re-rendering live in state. Pointer position is kept in a
// ref so we don't re-render every pointermove; the ghost DOM is positioned directly.
interface DragState {
  source: DragSource;
  startX: number;
  startY: number;
  dragging: boolean;
  target: DropTarget;
}

function targetsEqual(a: DropTarget, b: DropTarget): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  if (a.kind !== b.kind) return false;
  if (a.kind === "merge-with-project" && b.kind === "merge-with-project") return sameKey(a.path, b.path);
  if (a.kind === "merge-with-folder" && b.kind === "merge-with-folder") return a.folderId === b.folderId;
  if (a.kind === "reorder-top" && b.kind === "reorder-top") return a.index === b.index;
  if (a.kind === "reorder-in-folder" && b.kind === "reorder-in-folder") return a.folderId === b.folderId && a.index === b.index;
  return false;
}

export function Sidebar({ projects, projectIcons, selectedProject, activeCountByProject, sidebarLayout, onLayoutChange, onSelectProject, onGoHome, onRemoveProject, onEditProject, onHoverProject, onOpenSettings, onAddProject, onCollapse, activeTabId, linkedProjectKey, showRateLimit, showRateLimitCodex, updateAvailable }: SidebarProps) {
  const [ctx, setCtx] = useState<CtxState>(null);
  const [tooltip, setTooltip] = useState<{ text: string; rect: DOMRect } | null>(null);
  const [drag, setDrag] = useState<DragState | null>(null);
  const dragPosRef = useRef<{ x: number; y: number }>({ x: 0, y: 0 });
  const ghostRef = useRef<HTMLDivElement | null>(null);
  const [editingFolderId, setEditingFolderId] = useState<string | null>(null);
  const isHomeActive = activeTabId === "home" && !selectedProject;
  const hostsSnap = useHostsSnapshot();
  const isInTerminal = activeTabId !== "home" && activeTabId !== "settings";

  const projectsByPath = useMemo(() => {
    const m = new Map<string, ProjectInfo>();
    for (const p of projects) m.set(lookupKey(keyOf(p)), p);
    return m;
  }, [projects]);

  const showTooltip = useCallback((text: string, el: HTMLElement) => { setTooltip({ text, rect: el.getBoundingClientRect() }); }, []);
  const hideTooltip = useCallback(() => setTooltip(null), []);

  const saveFolder = useCallback((folderId: string, patch: { name?: string; color?: string | undefined }) => {
    const next = sidebarLayout.map(item => {
      if (!(item.kind === "folder" && item.id === folderId)) return item;
      const updated: SidebarFolder = { ...item };
      if (patch.name !== undefined) updated.name = patch.name;
      // Explicit handling for color: `undefined` means "use default" (strip the field).
      if ("color" in patch) {
        if (patch.color) updated.color = patch.color;
        else delete updated.color;
      }
      return updated;
    });
    onLayoutChange(next);
  }, [sidebarLayout, onLayoutChange]);

  const toggleFolder = useCallback((folderId: string) => {
    const next = sidebarLayout.map(item =>
      item.kind === "folder" && item.id === folderId ? { ...item, collapsed: !item.collapsed } : item
    );
    onLayoutChange(next);
  }, [sidebarLayout, onLayoutChange]);

  const handleFolderCtx = useCallback((e: React.MouseEvent, folder: SidebarFolder) => {
    e.preventDefault();
    setCtx({ kind: "folder", x: e.clientX, y: e.clientY, folder });
  }, []);

  const handleProjectCtx = useCallback((e: React.MouseEvent, project: ProjectInfo) => {
    e.preventDefault();
    setCtx({ kind: "project", x: e.clientX, y: e.clientY, project });
  }, []);

  const ungroupFolder = useCallback((folderId: string) => {
    // Replace the folder with its contained projects in place, preserving order.
    const next: SidebarItem[] = [];
    for (const item of sidebarLayout) {
      if (item.kind === "folder" && item.id === folderId) {
        for (const p of item.projectPaths) next.push({ kind: "project", path: p });
      } else {
        next.push(item);
      }
    }
    onLayoutChange(next);
  }, [sidebarLayout, onLayoutChange]);

  // ── Drag logic ──
  // Pointer-based (HTML5 DnD blocked in Tauri webview). Start tracking on pointerdown,
  // flip to "dragging" after ~6px of movement so clicks still work. elementFromPoint drives
  // hit-testing against data-drop-* attributes on rendered sidebar entries.
  const computeTarget = useCallback((x: number, y: number, source: DragSource): DropTarget => {
    const isFolderSource = source.kind === "folder";
    let el = document.elementFromPoint(x, y) as HTMLElement | null;
    while (el) {
      const ds = el.dataset;
      if (ds.dropKind) {
        if (ds.dropKind === "project" && ds.dropPath && ds.dropIdx != null) {
          const rect = el.getBoundingClientRect();
          const rel = (y - rect.top) / rect.height;
          const idx = parseInt(ds.dropIdx, 10);
          const parent = ds.dropParent || "";
          // Projects inside a folder NEVER offer a merge zone — we don't allow nested folders.
          // Also: when the source is a folder being reordered, the only valid target is top-level.
          if (isFolderSource && parent) { el = el.parentElement; continue; }
          if (parent) {
            // 50/50 split inside a folder — either reorder before or after this child.
            if (rel < 0.5) return { kind: "reorder-in-folder", folderId: parent, index: idx };
            return { kind: "reorder-in-folder", folderId: parent, index: idx + 1 };
          }
          // Top-level project. Three zones: before / merge / after.
          // But if the source is a folder, still no merge — folders don't nest.
          if (rel < 0.30) return { kind: "reorder-top", index: idx };
          if (rel > 0.70) return { kind: "reorder-top", index: idx + 1 };
          if (isFolderSource) return null;
          if (source.kind === "project" && !sameKey(ds.dropPath, source.path)) {
            return { kind: "merge-with-project", path: asProjectKey(ds.dropPath) };
          }
          // Dropping on yourself, middle-zone = no-op.
          return null;
        }
        // Outer folder container — covers the full folder bounds including padding above
        // and below the header. Zones are computed against the INNER HEADER's rect so
        // dropping in the top padding = "reorder before folder" (not "merge"), and in the
        // bottom padding or body area (for expanded folders) = merge-with-folder.
        if (ds.dropKind === "folder-outer" && ds.dropFolderId && ds.dropIdx != null) {
          const headerEl = (el as HTMLElement).querySelector(":scope > .ds-folder-header") as HTMLElement | null;
          if (!headerEl) { el = el.parentElement; continue; }
          const hr = headerEl.getBoundingClientRect();
          const idx = parseInt(ds.dropIdx, 10);
          const expanded = ds.dropExpanded === "1";
          // Self-drag: 50/50 split above/below header.
          if (isFolderSource && source.kind === "folder" && source.id === ds.dropFolderId) {
            return { kind: "reorder-top", index: y < hr.top + hr.height * 0.5 ? idx : idx + 1 };
          }
          // Top padding + top 30% of header = reorder before this folder.
          if (y < hr.top + hr.height * 0.30) return { kind: "reorder-top", index: idx };
          // For COLLAPSED folders: bottom 30% of header + bottom padding = reorder after.
          //   Middle 40% = merge.
          // For EXPANDED folders: only top 30% of header is reorder-before. Everything else
          //   (header middle/bottom + body + bottom padding) = merge with folder. Reorder-after
          //   is handled by the next sibling's top-30% zone.
          if (!expanded && y > hr.bottom - hr.height * 0.30) return { kind: "reorder-top", index: idx + 1 };
          if (isFolderSource) return null;
          return { kind: "merge-with-folder", folderId: ds.dropFolderId };
        }
        // Inside an expanded folder's body-wrap directly (gap below children) — fallback to
        // merge-with-folder. Legacy; kept for safety if folder-outer ever misses.
        if (ds.dropKind === "folder-body" && ds.dropFolderId) {
          if (isFolderSource) { el = el.parentElement; continue; }
          return { kind: "merge-with-folder", folderId: ds.dropFolderId };
        }
      }
      el = el.parentElement;
    }
    return null;
  }, []);

  const applyDrop = useCallback((source: DragSource, target: DropTarget) => {
    if (!target) return;

    // Branch 1: the source is a WHOLE FOLDER being reordered at the top level.
    if (source.kind === "folder") {
      if (target.kind !== "reorder-top") return;
      const idx = sidebarLayout.findIndex(i => i.kind === "folder" && i.id === source.id);
      if (idx < 0) return;
      const folder = sidebarLayout[idx];
      const stripped = [...sidebarLayout.slice(0, idx), ...sidebarLayout.slice(idx + 1)];
      // Target index was computed against the pre-strip layout; if we removed something before
      // the target, shift target index down by 1.
      const insertAt = Math.min(Math.max(0, target.index > idx ? target.index - 1 : target.index), stripped.length);
      if (insertAt === idx) return; // no-op
      const next = [...stripped.slice(0, insertAt), folder, ...stripped.slice(insertAt)];
      onLayoutChange(next);
      return;
    }

    // Branch 2: the source is a PROJECT being moved/merged.
    const sourcePath = source.path;
    const stripped: SidebarItem[] = [];
    for (const item of sidebarLayout) {
      if (item.kind === "project") {
        if (!sameKey(item.path, sourcePath)) stripped.push(item);
      } else {
        const kept = item.projectPaths.filter(p => !sameKey(p, sourcePath));
        stripped.push({ ...item, projectPaths: kept });
      }
    }
    let next: SidebarItem[] = stripped;
    let createdFolderId: string | null = null;
    if (target.kind === "merge-with-project") {
      next = stripped.map((item): SidebarItem => {
        if (item.kind === "project" && sameKey(item.path, target.path)) {
          const id = `sfolder-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 6)}`;
          createdFolderId = id;
          const folder: SidebarFolder = { kind: "folder", id, name: "New folder", collapsed: false, projectPaths: [item.path, sourcePath] };
          return folder;
        }
        return item;
      });
    } else if (target.kind === "merge-with-folder") {
      next = stripped.map((item): SidebarItem => {
        if (item.kind === "folder" && item.id === target.folderId) {
          return { ...item, projectPaths: [...item.projectPaths, sourcePath] };
        }
        return item;
      });
    } else if (target.kind === "reorder-top") {
      const clamped = Math.min(Math.max(0, target.index), stripped.length);
      next = [...stripped.slice(0, clamped), { kind: "project", path: sourcePath }, ...stripped.slice(clamped)];
    } else if (target.kind === "reorder-in-folder") {
      next = stripped.map((item): SidebarItem => {
        if (item.kind === "folder" && item.id === target.folderId) {
          const clamped = Math.min(Math.max(0, target.index), item.projectPaths.length);
          const paths = [...item.projectPaths.slice(0, clamped), sourcePath, ...item.projectPaths.slice(clamped)];
          return { ...item, projectPaths: paths };
        }
        return item;
      });
    }
    next = next.filter(item => item.kind !== "folder" || item.projectPaths.length > 0);
    onLayoutChange(next);
    if (createdFolderId) setEditingFolderId(createdFolderId);
  }, [sidebarLayout, onLayoutChange]);

  const startProjectDrag = useCallback((e: React.PointerEvent, path: ProjectKey, originFolderId: string | null) => {
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest("button, input")) return;
    dragPosRef.current = { x: e.clientX, y: e.clientY };
    setDrag({ source: { kind: "project", path, originFolderId }, startX: e.clientX, startY: e.clientY, dragging: false, target: null });
  }, []);

  const startFolderDrag = useCallback((e: React.PointerEvent, folderId: string) => {
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest("button, input")) return;
    dragPosRef.current = { x: e.clientX, y: e.clientY };
    setDrag({ source: { kind: "folder", id: folderId }, startX: e.clientX, startY: e.clientY, dragging: false, target: null });
  }, []);

  // Move/up handlers. Pos is written to a ref + directly to the ghost's style (no React render);
  // state only updates when `dragging` flips true or when the drop target actually changes.
  useEffect(() => {
    if (!drag) return;
    const moveGhost = () => {
      const g = ghostRef.current;
      if (!g) return;
      g.style.transform = `translate(${dragPosRef.current.x - 24}px, ${dragPosRef.current.y - 24}px) scale(1.05)`;
    };
    const onMove = (ev: PointerEvent) => {
      dragPosRef.current = { x: ev.clientX, y: ev.clientY };
      const dx = ev.clientX - drag.startX;
      const dy = ev.clientY - drag.startY;
      const dist = Math.hypot(dx, dy);
      if (!drag.dragging && dist < 6) return;
      // Ghost position — direct DOM manipulation, no React re-render.
      moveGhost();
      const target = computeTarget(ev.clientX, ev.clientY, drag.source);
      // Only setState when something observable changes (first transition to dragging,
      // or the drop target truly changed). Prevents per-pixel re-renders that caused flicker.
      if (!drag.dragging || !targetsEqual(drag.target, target)) {
        setDrag(prev => prev ? { ...prev, dragging: true, target } : null);
      }
    };
    const onUp = (ev: PointerEvent) => {
      const wasDragging = !!drag.dragging;
      const target = computeTarget(ev.clientX, ev.clientY, drag.source);
      setDrag(null);
      if (wasDragging && target) {
        applyDrop(drag.source, target);
        // Suppress the synthetic click that pointerup would otherwise fire on the source.
        const stopClick = (ce: MouseEvent) => { ce.stopPropagation(); ce.preventDefault(); window.removeEventListener("click", stopClick, true); };
        window.addEventListener("click", stopClick, true);
        setTimeout(() => window.removeEventListener("click", stopClick, true), 0);
      }
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    // Seed the ghost position on mount of this effect so it doesn't briefly paint at (0,0).
    moveGhost();
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, [drag, computeTarget, applyDrop]);

  // ── Render helpers ──
  // `path` is the item's ProjectKey (the bare path for a Local Project).
  const renderProject = (path: ProjectKey, idx: number, parentFolderId: string | null) => {
    const pl = lookupKey(path);
    const proj = projectsByPath.get(pl);
    const name = proj?.name || keyBasename(path);
    const settings = projectIcons[pl];
    const displayName = settings?.customName || name;
    const customIcon = settings?.icon;
    const isSelected = !isInTerminal && !!selectedProject && lookupKey(keyOf(selectedProject)) === pl;
    const isLinked = isInTerminal && !!linkedProjectKey && lookupKey(linkedProjectKey) === pl;
    const highlighted = isSelected || isLinked;
    const activeCount = activeCountByProject.get(pl) || 0;
    // Remote Project: Host badge; dimmed with a status dot while its Host is not usable.
    const host = proj?.host ?? parseProjectKey(path).host;
    const hostName = host ? (hostsSnap.configs.find(c => c.id === host)?.name || host) : "";
    const hostStatus = host ? hostsSnap.status[host] : undefined;
    const stale = !!host && !isUsableStatus(hostStatus);
    const tooltipText = !host ? displayName
      : stale ? `${fmt("sidebar.project.hostTooltip", { project: displayName, host: hostName })} · ${fmt("sidebar.project.staleTooltip", { host: hostName, status: statusLabel(hostStatus) })}`
      : fmt("sidebar.project.hostTooltip", { project: displayName, host: hostName });
    const isDragging = drag?.source.kind === "project" && lookupKey(drag.source.path) === pl;
    const isMergeTarget = drag?.target?.kind === "merge-with-project" && lookupKey(drag.target.path) === pl;
    // Drop-line indicators on the item itself — cleaner than explicit gap strips.
    const indicatorBefore = drag?.target && (
      (parentFolderId && drag.target.kind === "reorder-in-folder" && drag.target.folderId === parentFolderId && drag.target.index === idx)
      || (!parentFolderId && drag.target.kind === "reorder-top" && drag.target.index === idx)
    );
    const indicatorAfter = drag?.target && (
      (parentFolderId && drag.target.kind === "reorder-in-folder" && drag.target.folderId === parentFolderId && drag.target.index === idx + 1)
      || (!parentFolderId && drag.target.kind === "reorder-top" && drag.target.index === idx + 1)
    );
    return (
      <div
        key={`p-${path}`}
        data-drop-kind="project"
        data-drop-path={path}
        data-drop-idx={idx}
        data-drop-parent={parentFolderId || ""}
        className={`ds-item ${isSelected ? "active" : ""} ${isLinked ? "linked" : ""} ${isDragging ? "ds-dragging" : ""} ${isMergeTarget ? "ds-merge-target" : ""} ${stale ? "ds-stale" : ""}`}
        onClick={(e) => { if (!drag?.dragging && proj) onSelectProject(proj); e.stopPropagation(); }}
        onContextMenu={(e) => proj && handleProjectCtx(e, proj)}
        onMouseEnter={(e) => { onHoverProject(path); showTooltip(tooltipText, e.currentTarget); }}
        onMouseLeave={() => { onHoverProject(null); hideTooltip(); }}
        onPointerDown={(e) => startProjectDrag(e, path, parentFolderId)}
      >
        {indicatorBefore && <div className="ds-drop-line ds-drop-line-before" />}
        <ProjectSidebarIcon iconValue={customIcon} color={settings?.color} name={displayName} highlighted={highlighted} />
        {host && <HostBadge host={host} size="md" className="ds-host-badge" />}
        {stale && <div className={`ds-host-status ds-host-status-${hostStatus?.status ?? "reconnecting"}`} />}
        {activeCount > 0 && <div className="ds-active-badge" title={`${activeCount} active session${activeCount > 1 ? "s" : ""}`}>{activeCount}</div>}
        <div className="ds-indicator" />
        {indicatorAfter && <div className="ds-drop-line ds-drop-line-after" />}
      </div>
    );
  };

  return (
    <>
      <div className="discord-sidebar">
        <div className={`ds-item ${isHomeActive ? "active" : ""}`} onClick={onGoHome} onMouseEnter={(e) => showTooltip("Home", e.currentTarget)} onMouseLeave={hideTooltip}>
          <div className="ds-icon ds-home"><img className="ds-home-logo" src={logo} alt="" /></div>
          <div className="ds-indicator" />
        </div>

        <div className="ds-separator" />

        <div className="ds-scroll">
          {sidebarLayout.map((item, idx) => {
            if (item.kind === "project") {
              return renderProject(item.path, idx, null);
            }
            // Folder entry
            const folder = item;
            const isMergeTarget = drag?.target?.kind === "merge-with-folder" && drag.target.folderId === folder.id;
            const isDraggingSelf = drag?.source.kind === "folder" && drag.source.id === folder.id;
            const folderActiveCount = folder.projectPaths.reduce((acc, p) => acc + (activeCountByProject.get(lookupKey(p)) || 0), 0);
            const tooltipText = folder.name + (folder.projectPaths.length ? ` (${folder.projectPaths.length})` : "");
            // Drop-line indicators around the folder header (top-level reorder hits).
            const folderIndicatorBefore = drag?.target?.kind === "reorder-top" && drag.target.index === idx;
            const folderIndicatorAfter = drag?.target?.kind === "reorder-top" && drag.target.index === idx + 1;
            return (
              <div
                key={folder.id}
                className={`ds-flow ds-folder ${folder.color ? "ds-folder-colored" : ""} ${folder.collapsed ? "ds-folder-collapsed" : "ds-folder-open"} ${isMergeTarget && !folder.collapsed ? "ds-folder-merge-target" : ""} ${isDraggingSelf ? "ds-dragging" : ""}`}
                style={folder.color ? ({ ["--folder-color" as any]: folder.color } as React.CSSProperties) : undefined}
                data-drop-kind="folder-outer"
                data-drop-folder-id={folder.id}
                data-drop-idx={idx}
                data-drop-expanded={folder.collapsed ? "0" : "1"}
              >
                {folderIndicatorBefore && <div className="ds-drop-line ds-drop-line-before ds-drop-line-folder" />}
                <div
                  className={`ds-folder-header ${isMergeTarget && folder.collapsed ? "ds-merge-target" : ""}`}
                  onClick={() => { if (!drag?.dragging) toggleFolder(folder.id); }}
                  onContextMenu={(e) => handleFolderCtx(e, folder)}
                  onMouseEnter={(e) => showTooltip(tooltipText, e.currentTarget)}
                  onMouseLeave={hideTooltip}
                  onPointerDown={(e) => startFolderDrag(e, folder.id)}
                >
                  {folder.collapsed ? (
                    <FolderMiniGrid paths={folder.projectPaths} projectIcons={projectIcons} projects={projects} />
                  ) : (
                    <div className="ds-folder-open-icon"><FolderIcon size={18} /></div>
                  )}
                  {/* Rollup count only shown when collapsed — when the folder is open, each
                      contained project already shows its own badge, so this would be noise. */}
                  {folder.collapsed && folderActiveCount > 0 && <div className="ds-active-badge" title={`${folderActiveCount} active session${folderActiveCount > 1 ? "s" : ""}`}>{folderActiveCount}</div>}
                </div>
                <div
                  className={`ds-folder-body-wrap ${folder.collapsed ? "collapsed" : ""}`}
                  data-drop-kind="folder-body"
                  data-drop-folder-id={folder.id}
                >
                  <div className="ds-folder-body">
                    {folder.projectPaths.map((p, i) => renderProject(p, i, folder.id))}
                  </div>
                </div>
                {folderIndicatorAfter && <div className="ds-drop-line ds-drop-line-after ds-drop-line-folder" />}
              </div>
            );
          })}
          {drag?.target?.kind === "reorder-top" && drag.target.index === sidebarLayout.length && (
            <div className="ds-drop-line ds-drop-line-standalone" />
          )}
          <div className="ds-item" onClick={onAddProject} onMouseEnter={(e) => showTooltip("Add Project", e.currentTarget)} onMouseLeave={hideTooltip}>
            <div className="ds-icon ds-add"><Plus size={16} /></div>
          </div>
        </div>

        {/* Account-wide rate-limit chip (5h / 7d window). Sourced from the freshest
            xshell-stats file across all sessions — same number Claude Code shows on its
            statusline; Codex's come from its rollout files. The indicator renders nothing
            when neither agent is enabled or has data, so gate the separator on the same
            "either could show" condition. Settings itself now lives in the top bar. */}
        {(showRateLimit || showRateLimitCodex) && <><div className="ds-separator" /><RateLimitIndicator showClaude={showRateLimit} showCodex={showRateLimitCodex} /></>}
      </div>

      {/* Floating ghost that follows the cursor during drag — positioned via direct DOM
          manipulation (transform) in the move handler so we don't re-render every pointermove. */}
      {drag?.dragging && (() => {
        const commonProps = { ref: ghostRef, className: "ds-drag-ghost" } as const;
        if (drag.source.kind === "project") {
          const pl = lookupKey(drag.source.path);
          const proj = projectsByPath.get(pl);
          const settings = projectIcons[pl];
          const displayName = settings?.customName || proj?.name || parseProjectKey(drag.source.path).path.split(/[\\/]/).pop() || "Project";
          return (
            <div {...commonProps}>
              <ProjectSidebarIcon iconValue={settings?.icon} color={settings?.color} name={displayName} highlighted={false} />
            </div>
          );
        }
        const folder = sidebarLayout.find(i => i.kind === "folder" && i.id === (drag.source as { kind: "folder"; id: string }).id) as SidebarFolder | undefined;
        if (!folder) return null;
        return (
          <div {...commonProps}>
            <div className="ds-folder-header"><FolderMiniGrid paths={folder.projectPaths} projectIcons={projectIcons} projects={projects} /></div>
          </div>
        );
      })()}

      {tooltip && !drag?.dragging && <Tooltip text={tooltip.text} rect={tooltip.rect} />}
      {ctx?.kind === "project" && <ProjectContextMenu ctx={ctx} onEdit={() => onEditProject(keyOf(ctx.project))} onReveal={ctx.project.host ? null : () => invoke("reveal_in_explorer", { path: ctx.project.path }).catch(() => {})} onRemove={() => onRemoveProject(keyOf(ctx.project))} onClose={() => setCtx(null)} />}
      {ctx?.kind === "folder" && <FolderContextMenu ctx={ctx} onRename={() => setEditingFolderId(ctx.folder.id)} onUngroup={() => ungroupFolder(ctx.folder.id)} onClose={() => setCtx(null)} />}
      {editingFolderId && (() => {
        const folder = sidebarLayout.find(i => i.kind === "folder" && i.id === editingFolderId) as SidebarFolder | undefined;
        if (!folder) { setEditingFolderId(null); return null; }
        return <FolderEditorDialog folder={folder} onSave={({ name, color }) => saveFolder(folder.id, { name, color })} onClose={() => setEditingFolderId(null)} />;
      })()}
    </>
  );
}
