import { createContext, useContext, useEffect, useMemo, useRef, useState, useCallback, useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import { save as saveDialog, open as openDialog } from "@tauri-apps/plugin-dialog";
import { fmt, type StringKey } from "../hosts/strings";
import { hostInvoke } from "../hosts/hostInvoke";
import { registry } from "../hosts/registry";
import type { HostId } from "../hosts/types";
import { ChevronRight, RefreshCw, Folder, ArrowUp, Search, X as XIcon, FolderOpen, Terminal as TerminalIcon, ExternalLink, Download, GripVertical } from "lucide-react";
import { baseName, isTransferPart, parentOf, styleOfOs } from "../files/paths";
import type { SyncState, SyncView, SyncNotice } from "../files/syncBack";
import { syncBack } from "../files/syncBackDeps";
import { fitNotice } from "../files/noticeText";
import { DRAG_PATH_MIME, acceptsDrop, resolveDropTarget, rowData } from "../files/dropTarget";
import { clashResolver, uploadSummary, type ClashAnswer } from "../files/clash";
import { downloadTo, type DownloadToDeps } from "../files/downloadTo";
import { DRAG_OUT_MAX_BYTES, type DragOutError } from "../files/dragOut";
import { dragOut } from "../files/dragOutDeps";
import * as transfer from "../files/transfer";
import type { DirItem } from "../types";
import { AgentIcon, type AgentId } from "../agents";
import { fileIconUrl, folderIconUrl } from "../lib/fileIcons";

// Agent config folders get their brand icon instead of a generic folder — a little flourish
// so `.claude` / `.codex` / `.cursor` / `.opencode` / `.agents` (Antigravity) are recognizable
// at a glance. Matched by exact name.
const AGENT_DIRS: Record<string, AgentId> = { ".claude": "claude", ".codex": "codex", ".cursor": "cursor", ".opencode": "opencode", ".agents": "antigravity" };
function agentForDir(item: DirItem): AgentId | null { return item.is_dir ? (AGENT_DIRS[item.name.toLowerCase()] ?? null) : null; }

// Hover-tooltip delay for tree rows — they shouldn't pop the instant the cursor crosses a
// row (that's noisy when scanning a folder). Header action buttons keep an instant tooltip.
const TOOLTIP_DELAY_MS = 1000;

// Dnd mime carrying an item's absolute path (row → terminal); see files/dropTarget.ts.
export { DRAG_PATH_MIME };

// Wrap paths containing whitespace in double quotes so they survive being typed at a shell
// prompt (Windows paths routinely have spaces). A trailing space lets the user keep typing.
function pathForTerminal(p: string): string { return /\s/.test(p) ? `"${p}" ` : `${p} `; }

// Listings without the hidden part files of uploads in progress.
const listable = (items: DirItem[]) => items.filter((i) => !isTransferPart(i.name));

// Directory portion of a search hit, relative to the search root — shown dimmed after the
// name so the user can tell two same-named files apart. Empty for top-level hits.
function relDir(fullPath: string, root: string, name: string): string {
  let p = fullPath.startsWith(root) ? fullPath.slice(root.length) : fullPath;
  p = p.replace(/^[\\/]+/, "");
  if (p.toLowerCase().endsWith(name.toLowerCase())) p = p.slice(0, p.length - name.length);
  return p.replace(/[\\/]+$/, "");
}


interface RowProps {
  item: DirItem;
  depth: number;
  expanded?: boolean;      // undefined → no chevron (files, and flat search hits)
  active: boolean;         // right-click target — kept highlighted while its menu is open
  subtitle?: string;
  onActivate: () => void;
  onContext: (x: number, y: number, item: DirItem) => void;
  showTt: (text: string, el: HTMLElement) => void;
  hideTt: () => void;
}

// The Remote Host's name inside its explorer (null on the Local Host): files there open as
// local copies whose saves upload back, and cannot be revealed.
const RemoteRows = createContext<string | null>(null);

// This explorer's Host (undefined: the Local Host) and where its grips report problems.
const GripEnv = createContext<{ host?: HostId; onError: (e: DragOutError) => void }>({ onError: () => {} });

// The folder an OS drag over the explorer would upload into (a folder row is highlighted).
const DropPath = createContext<string | null>(null);

// Sync-back state of the files of this explorer's Host that are open, by path.
const SyncStates = createContext<ReadonlyMap<string, SyncState>>(new Map());

// The row badge of an open copy: busy, waiting for the Host, or needing the user.
function syncBadge(state: SyncState | undefined): "syncing" | "waiting" | "attention" | null {
  switch (state) {
    case "downloading": case "settling": case "hashing": case "uploading": return "syncing";
    case "waitingHost": case "unknownOutcome": return "waiting";
    case "conflict": case "failed": case "missing": return "attention";
    default: return null;
  }
}

// Single presentational row, shared by the tree and the flat search results.
function FileRow({ item, depth, expanded, active, subtitle, onActivate, onContext, showTt, hideTt }: RowProps) {
  const remote = useContext(RemoteRows);
  const badge = syncBadge(useContext(SyncStates).get(item.path));
  const dropHere = useContext(DropPath) === item.path && item.is_dir;
  const agent = agentForDir(item);
  const iconUrl = agent ? "" : item.is_dir ? folderIconUrl(item.name, !!expanded) : fileIconUrl(item.name);
  const tip = item.is_dir ? item.path : remote !== null ? fmt("explorer.row.tooltipRemote", { host: remote }) : fmt("explorer.row.tooltipFile", { path: item.path });
  const onDragStart = (e: React.DragEvent) => {
    // The grip holds the press for a native drag-out: no in-app drag then.
    if (dragOut.held) { e.preventDefault(); return; }
    e.dataTransfer.setData(DRAG_PATH_MIME, item.path);
    e.dataTransfer.setData("text/plain", item.path);
    e.dataTransfer.effectAllowed = "copy";
  };
  return (
    <div
      className={`file-row ${active ? "file-row-active" : ""} ${dropHere ? "file-row-drop" : ""}`}
      style={{ paddingLeft: 6 + depth * 12 }}
      data-path={item.path}
      data-dir={item.is_dir ? "1" : "0"}
      draggable
      onClick={onActivate}
      onDragStart={onDragStart}
      onContextMenu={(e) => { e.preventDefault(); e.stopPropagation(); onContext(e.clientX, e.clientY, item); }}
      onMouseEnter={(e) => showTt(tip, e.currentTarget)}
      onMouseLeave={hideTt}
    >
      <span className="file-chev">{item.is_dir && expanded !== undefined && <ChevronRight size={12} className={expanded ? "open" : ""} />}</span>
      {agent ? <AgentIcon agent={agent} size={16} className="file-icon" /> : <img className="file-icon" src={iconUrl} alt="" draggable={false} />}
      <span className="file-name">{item.name}</span>
      {subtitle && <span className="file-row-sub">{subtitle}</span>}
      {badge && <span className={`file-row-sync file-row-sync-${badge}`} aria-hidden />}
      <Grip item={item} showTt={showTt} hideTt={hideTt} />
    </div>
  );
}

const MAX_DRAG_LABEL = `${DRAG_OUT_MAX_BYTES / (1024 * 1024)} MB`;

// The drag handle at a row's right edge: a press, a small move with the button still held,
// and the file (a prepared copy on a Remote Host) is dragged out to the OS. The rest of the
// row keeps its in-app drag to the terminal.
function Grip({ item, showTt, hideTt }: { item: DirItem; showTt: (text: string, el: HTMLElement) => void; hideTt: () => void }) {
  const { host, onError } = useContext(GripEnv);
  useSyncExternalStore(dragOut.subscribe, dragOut.getSnapshot);
  const st = dragOut.grip(host, item.path, item.is_dir);
  const tip = st.kind === "disabled"
    ? (st.reason === "tooLarge" ? fmt("explorer.grip.tooLarge", { max: MAX_DRAG_LABEL }) : fmt("explorer.grip.folderRemote"))
    : st.kind === "preparing" ? fmt("explorer.grip.preparing", { name: item.name, pct: st.pct }) : fmt("explorer.grip.tooltip");
  return (
    <span
      className={`file-row-grip file-row-grip-${st.kind}`}
      draggable={false}
      aria-label={tip}
      aria-disabled={st.kind === "disabled"}
      onMouseEnter={(e) => { e.stopPropagation(); showTt(tip, e.currentTarget); void dragOut.probe(host, item.path, item.is_dir); }}
      onMouseLeave={hideTt}
      onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); }}
      onClick={(e) => e.stopPropagation()}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        e.stopPropagation();
        hideTt();
        try { e.currentTarget.setPointerCapture(e.pointerId); } catch { /* not capturable: moves still arrive over the grip */ }
        dragOut.down(host, item.path, item.is_dir, e.clientX, e.clientY, onError);
      }}
      onPointerMove={(e) => dragOut.move(e.clientX, e.clientY)}
      onPointerUp={() => dragOut.up()}
      onPointerCancel={() => dragOut.up()}
      onLostPointerCapture={() => dragOut.up()}
    >
      {st.kind === "preparing" ? <span className="file-row-grip-pct">{st.pct}%</span> : <GripVertical size={12} />}
    </span>
  );
}

interface NodeProps {
  item: DirItem;
  depth: number;
  tick: number;
  host?: HostId;
  activePath: string | null;
  onOpen: (path: string) => void;
  onContext: (x: number, y: number, item: DirItem) => void;
  showTt: (text: string, el: HTMLElement) => void;
  hideTt: () => void;
}

// One tree row + its lazily-loaded children. Children load on first expand and then stay
// mounted (height-collapsed) so the open/close height transition is smooth and re-expanding
// is instant. The grid-rows trick (0fr↔1fr) animates to the children's natural height.
function Node({ item, depth, tick, host, activePath, onOpen, onContext, showTt, hideTt }: NodeProps) {
  const [expanded, setExpanded] = useState(false);
  const [children, setChildren] = useState<DirItem[] | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!expanded || children !== null) return;
    let alive = true;
    setLoading(true);
    hostInvoke<DirItem[]>(host, "list_dir", { path: item.path })
      .then((c) => { if (alive) setChildren(listable(c)); })
      .catch(() => { if (alive) setChildren([]); })
      .finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [expanded, item.path, children, host]);

  // Silent re-pull on each poll tick (only for an open, already-loaded folder) so additions /
  // deletions inside it show up live. Collapsed or unloaded folders are skipped.
  useEffect(() => {
    if (tick === 0 || !expanded || children === null) return;
    let alive = true;
    hostInvoke<DirItem[]>(host, "list_dir", { path: item.path }).then((c) => { if (alive) setChildren(listable(c)); }).catch(() => {});
    return () => { alive = false; };
  }, [tick]);

  const onActivate = () => { if (item.is_dir) setExpanded((v) => !v); else onOpen(item.path); };
  const childPad = 6 + (depth + 1) * 12 + 14;

  return (
    <>
      <FileRow item={item} depth={depth} expanded={item.is_dir ? expanded : undefined} active={activePath === item.path} onActivate={onActivate} onContext={onContext} showTt={showTt} hideTt={hideTt} />
      {item.is_dir && (
        <div className={`file-children ${expanded ? "open" : ""}`}>
          <div className="file-children-inner">
            {children === null
              ? (loading && expanded ? <div className="file-row file-row-muted" style={{ paddingLeft: childPad }}>Loading…</div> : null)
              : children.length === 0
                ? <div className="file-row file-row-muted" style={{ paddingLeft: childPad }}>Empty</div>
                : children.map((c) => <Node key={c.path} item={c} depth={depth + 1} tick={tick} host={host} activePath={activePath} onOpen={onOpen} onContext={onContext} showTt={showTt} hideTt={hideTt} />)}
          </div>
        </div>
      )}
    </>
  );
}

interface PanelProps {
  rootPath: string;
  host?: HostId;
  // Types the given text into this panel's terminal (local PTY or Remote Terminal).
  onWritePath: (data: string) => void;
  visible: boolean;
  showTt: (text: string, el: HTMLElement) => void;
  hideTt: () => void;
}

// Inner content of the file-explorer side panel — header (current dir + up/search/refresh) and
// a scrollable area showing either the lazy tree or flat search results. TerminalTab wraps this
// in the shared `.terminal-side-panel` + splitter, mirroring how the git panel is hosted.
export function FileExplorerPanel({ rootPath, host, onWritePath, visible, showTt, hideTt }: PanelProps) {
  // The browsable root. Starts at the terminal's cwd but the up-button can climb past it.
  const [cwd, setCwd] = useState(rootPath);
  const [roots, setRoots] = useState<DirItem[] | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [ctx, setCtx] = useState<{ x: number; y: number; item: DirItem } | null>(null);

  const [searchOpen, setSearchOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<DirItem[] | null>(null);
  const [searching, setSearching] = useState(false);
  const searchInputRef = useRef<HTMLInputElement>(null);

  // Reset to the terminal's cwd if the tab's project path changes underneath us.
  // Keyed on (host, path): the same path on another Host is a different folder (amendment 23).
  useEffect(() => { setCwd(rootPath); setRoots(null); }, [rootPath, host]);

  const load = useCallback((silent = false) => {
    if (!silent) setRefreshing(true);
    hostInvoke<DirItem[]>(host, "list_dir", { path: cwd })
      .then((items) => setRoots(listable(items)))
      .catch(() => setRoots([]))
      .finally(() => { if (!silent) setRefreshing(false); });
  }, [cwd, host]);

  useEffect(() => { load(); }, [load]);

  // Poll for filesystem changes while the panel is visible (the agent adds/removes files, etc.)
  // so the tree updates without a manual refresh. `tick` is threaded into every Node so expanded
  // sub-folders re-pull too; list_dir is async (off the UI thread) so this stays cheap. Silent
  // (no spinner) and paused while hidden or searching.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!visible || searchOpen) return;
    setTick(t => t + 1); // refresh right away on (re)show, then keep polling
    const id = window.setInterval(() => setTick(t => t + 1), 2500);
    return () => window.clearInterval(id);
  }, [visible, searchOpen]);
  const loadRef = useRef(load);
  loadRef.current = load;
  useEffect(() => { if (tick > 0) loadRef.current(true); }, [tick]);

  // Debounced recursive search rooted at the current dir; empty query falls back to the tree.
  useEffect(() => {
    if (!searchOpen) return;
    const q = query.trim();
    if (!q) { setResults(null); setSearching(false); return; }
    setSearching(true);
    const t = window.setTimeout(() => {
      hostInvoke<DirItem[]>(host, "search_dir", { root: cwd, query: q, limit: 300 })
        .then((items) => setResults(listable(items))).catch(() => setResults([])).finally(() => setSearching(false));
    }, 220);
    return () => window.clearTimeout(t);
  }, [query, searchOpen, cwd, host]);

  useEffect(() => { if (searchOpen) searchInputRef.current?.focus(); }, [searchOpen]);

  // Escape closes the context menu first, then the search box.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (ctx) { setCtx(null); return; }
      if (searchOpen) { setSearchOpen(false); setQuery(""); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [ctx, searchOpen]);

  // Reveal is Local only; on a Remote Host it does nothing.
  const reveal = useCallback((path: string) => { if (host) return; invoke("reveal_in_explorer", { path }).catch(() => {}); }, [host]);
  const writePath = useCallback((path: string) => { onWritePath(pathForTerminal(path)); }, [onWritePath]);
  // Open a file in its default app: the file itself on the Local Host, a local copy whose saves
  // upload back on a Remote Host. This explorer owns the copies it opens; when it goes away,
  // the clean ones are deleted.
  const ownerRef = useRef<string>(crypto.randomUUID());
  useEffect(() => { const owner = ownerRef.current; return () => syncBack.releaseOwner(owner); }, []);
  const openFile = useCallback((path: string) => { void syncBack.open(host, path, ownerRef.current); }, [host]);
  const sync = useSyncExternalStore(syncBack.subscribe, syncBack.getSnapshot);
  const syncStates = useMemo(() => new Map(host ? sync.sessions.filter((v) => v.host === host).map((v) => [v.remotePath, v.state] as const) : []), [sync, host]);
  const hostName = host ? registry.hostName(host) : null;
  const openContext = useCallback((x: number, y: number, item: DirItem) => setCtx({ x, y, item }), []);

  // Tooltip with a hover delay (tree/search rows). The timer is cleared on leave so it never
  // fires for a row the cursor has already left.
  const ttTimer = useRef<number | null>(null);
  const showTtDelayed = useCallback((text: string, el: HTMLElement) => {
    if (ttTimer.current) window.clearTimeout(ttTimer.current);
    ttTimer.current = window.setTimeout(() => showTt(text, el), TOOLTIP_DELAY_MS);
  }, [showTt]);
  const hideTtNow = useCallback(() => { if (ttTimer.current) window.clearTimeout(ttTimer.current); hideTt(); }, [hideTt]);
  useEffect(() => () => { if (ttTimer.current) window.clearTimeout(ttTimer.current); }, []);

  // The Host's path rules when its OS is known, else read from the path's form.
  const pathRules = host ? styleOfOs(registry.getStatus(host)?.os) : undefined;
  const parent = parentOf(cwd, pathRules);

  // ── Transfers: drop-in uploads (D4), Download to… (D3), drag-out errors ──
  const [jobs, setJobs] = useState<Job[]>([]);
  const [notices, setNotices] = useState<NoticeItem[]>([]);
  const [clash, setClash] = useState<ClashPrompt | null>(null);
  const clashChain = useRef<Promise<unknown>>(Promise.resolve());
  const alive = useRef(true);
  // Every transfer of this explorer: closing it (the Tab) cancels them, which also answers a
  // waiting clash question and drops an upload the Host still holds.
  const controllers = useRef(new Set<AbortController>());
  useEffect(() => {
    alive.current = true;
    const live = controllers.current;
    return () => { alive.current = false; for (const c of live) c.abort(); live.clear(); };
  }, []);
  const newController = useCallback(() => { const ac = new AbortController(); controllers.current.add(ac); return ac; }, []);
  const noticeTimers = useRef<number[]>([]);
  useEffect(() => () => { for (const t of noticeTimers.current) window.clearTimeout(t); }, []);
  const dismissNotice = useCallback((id: string) => setNotices((ns) => ns.filter((n) => n.id !== id)), []);
  const pushNotice = useCallback((key: StringKey, params: Record<string, string | number>, tone: NoticeItem["tone"], full?: string) => {
    if (!alive.current) return;
    const id = `t${crypto.randomUUID()}`;
    const fit = fitNotice(key, params);
    setNotices((ns) => [...ns, { id, text: fit.text, full: full ?? fit.full, tone, actions: [{ label: fmt("explorer.notice.dismiss"), run: () => dismissNotice(id) }] }]);
    if (tone === "info") noticeTimers.current.push(window.setTimeout(() => dismissNotice(id), NOTICE_INFO_MS));
  }, [dismissNotice]);
  const setJob = useCallback((id: string, patch: Partial<Job> & { cancel?: () => void }) => {
    if (!alive.current) return;
    setJobs((js) => js.some((j) => j.id === id) ? js.map((j) => (j.id === id ? { ...j, ...patch } : j)) : [...js, { id, text: "", pct: 0, cancel: () => {}, ...patch }]);
  }, []);
  const endJob = useCallback((id: string) => { if (alive.current) setJobs((js) => js.filter((j) => j.id !== id)); }, []);

  // One clash dialog at a time; a cancelled batch's question answers "skip".
  const askClash = useCallback((path: string, folderOf: string, signal: AbortSignal): Promise<ClashAnswer> => {
    const next = clashChain.current.then(() => new Promise<ClashAnswer>((resolve) => {
      if (signal.aborted || !alive.current) { resolve({ choice: "skip", applyAll: false }); return; }
      const done = (a: ClashAnswer) => { signal.removeEventListener("abort", onAbort); if (alive.current) setClash(null); resolve(a); };
      const onAbort = () => done({ choice: "skip", applyAll: false });
      signal.addEventListener("abort", onAbort, { once: true });
      setClash({ name: baseName(path, pathRules), folder: folderOf, answer: done });
    }));
    clashChain.current = next.catch(() => {});
    return next;
  }, [pathRules]);

  // Upload what an OS drop holds: its job (with Cancel) shows while the dropped folders are
  // still being read.
  const runUpload = useCallback(async (target: string, dt: DataTransfer) => {
    const id = crypto.randomUUID();
    const ac = newController();
    const folder = baseName(target, pathRules);
    const listing = (found: number) => setJob(id, { text: fmt("explorer.transfer.listing", { name: folder, n: found }), pct: 0 });
    setJob(id, { text: fmt("explorer.transfer.listing", { name: folder, n: 0 }), pct: 0, cancel: () => ac.abort() });
    // The items are readable only now, synchronously; the folder walk itself is async.
    let read: Promise<transfer.UploadEntry[]>;
    try { read = transfer.entriesFromDrop(dt, { signal: ac.signal, onFound: listing }); } catch (err) { read = Promise.reject(err); }
    try {
      let entries: transfer.UploadEntry[];
      try {
        entries = await read;
      } catch (e) {
        if (e instanceof transfer.TransferAbortedError || ac.signal.aborted) throw new transfer.TransferAbortedError(0);
        pushNotice("explorer.drop.folderUnsupported", {}, "error");
        return;
      }
      if (entries.length === 0) return;
      const files = entries.filter((e) => e.file);
      const first = files[0]?.rel ?? entries[0].rel;
      setJob(id, { text: fmt("explorer.drop.uploading", { i: files.length ? 1 : 0, n: files.length, name: first[first.length - 1], pct: 0 }), pct: 0 });
      const resolveClash = clashResolver((p) => askClash(p, folder, ac.signal));
      const r = await transfer.uploadEntries(host, target, entries, { resolveClash }, (_d, _t, current, at) => {
        if (!at) return;
        const pct = at.fileSize > 0 ? Math.round((at.fileDone / at.fileSize) * 100) : 100;
        setJob(id, { text: fmt("explorer.drop.uploading", { i: at.i, n: at.n, name: baseName(current, pathRules), pct }), pct });
      }, ac.signal);
      const sum = uploadSummary(r, folder);
      const details = [...r.skipped.map((p) => `${p}: skipped`), ...r.failed.map((f) => `${f.path}: ${f.error}`)].join("\n");
      pushNotice(sum.key, sum.params, sum.tone, details ? `${fitNotice(sum.key, sum.params).full}\n${details}` : undefined);
    } catch (e) {
      if (e instanceof transfer.TransferAbortedError) pushNotice("explorer.transfer.cancelled", { n: e.completed }, "info");
      else pushNotice("explorer.drop.partial", { n: 0, skipped: 0, failed: 1 }, "error", transfer.messageOf(e));
    } finally {
      controllers.current.delete(ac);
      endJob(id);
      // Re-list the folder and every expanded one (a tick re-pulls them all).
      if (alive.current) setTick((t) => t + 1);
    }
  }, [host, pathRules, setJob, endJob, pushNotice, askClash, newController]);

  const runDownload = useCallback(async (item: DirItem) => {
    const id = crypto.randomUUID();
    const ac = newController();
    const cancel = () => ac.abort();
    const deps: DownloadToDeps = {
      saveDialog: (name) => saveDialog({ defaultPath: name }),
      folderDialog: async () => { const r = await openDialog({ directory: true, multiple: false }); return typeof r === "string" ? r : null; },
      reserveDir: (path) => invoke<boolean>("transfer_reserve_dir", { path }),
      downloadFile: (remote, local, onProgress, signal) => transfer.download(host, remote, local, onProgress, signal),
      downloadTree: (remote, local, onProgress, signal, onListing) => transfer.downloadTree(host, remote, local, onProgress, signal, onListing),
    };
    try {
      const r = await downloadTo({ path: item.path, name: item.name, isDir: item.is_dir }, deps, {
        progress: (i, n, name, pct) => setJob(id, { text: fmt("explorer.download.progress", { i, n, name, pct }), pct, cancel }),
        listing: (name, n) => setJob(id, { text: fmt("explorer.transfer.listing", { name, n }), pct: 0, cancel }),
      }, ac.signal);
      if (r && r.skipped.length > 0) {
        const full = `${fitNotice("explorer.download.partial", { n: r.files, path: r.path, skipped: r.skipped.length }).full}\n${r.skipped.join("\n")}`;
        pushNotice("explorer.download.partial", { n: r.files, path: r.path, skipped: r.skipped.length }, "warn", full);
      } else if (r) pushNotice("explorer.download.done", { path: r.path }, "info");
    } catch (e) {
      if (e instanceof transfer.TransferAbortedError) pushNotice("explorer.transfer.cancelled", { n: e.completed }, "info");
      else if (e instanceof transfer.TreeTooLargeError) pushNotice("explorer.download.tooMany", { max: e.max }, "error", e.message);
      else pushNotice("explorer.download.failed", { name: item.name, error: transfer.messageOf(e) }, "error");
    } finally {
      controllers.current.delete(ac);
      endJob(id);
    }
  }, [host, setJob, endJob, pushNotice, newController]);

  // Drag-out problems of this explorer's grips surface here.
  const onDragOutError = useCallback((e: DragOutError) => {
    if (e.kind === "start") pushNotice("explorer.drag.unsupported", {}, "error", `${fmt("explorer.drag.unsupported")}\n${transfer.messageOf(e.error)}`);
    else pushNotice("explorer.download.failed", { name: baseName(e.path, pathRules), error: transfer.messageOf(e.error) }, "error");
  }, [pushNotice, pathRules]);

  const gripEnv = useMemo(() => ({ host, onError: onDragOutError }), [host, onDragOutError]);

  // OS drags over the tree: highlight where they would go; drop to upload. An enter/leave
  // count keeps the highlight steady while the pointer crosses rows.
  const [dropPath, setDropPath] = useState<string | null>(null);
  const dragDepth = useRef(0);
  const dropHandlers = {
    onDragEnter: (e: React.DragEvent) => {
      if (!acceptsDrop(e.dataTransfer?.types, dragOut.active)) return;
      e.preventDefault();
      dragDepth.current++;
      setDropPath(resolveDropTarget(rowData(e.target as Element), cwd));
    },
    onDragOver: (e: React.DragEvent) => {
      if (!acceptsDrop(e.dataTransfer?.types, dragOut.active)) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = "copy";
      const t = resolveDropTarget(rowData(e.target as Element), cwd);
      setDropPath((cur) => (cur === t ? cur : t));
    },
    onDragLeave: (e: React.DragEvent) => {
      if (!acceptsDrop(e.dataTransfer?.types, dragOut.active)) return;
      dragDepth.current = Math.max(0, dragDepth.current - 1);
      if (dragDepth.current === 0) setDropPath(null);
    },
    onDrop: (e: React.DragEvent) => {
      dragDepth.current = 0;
      setDropPath(null);
      if (!acceptsDrop(e.dataTransfer?.types, dragOut.active)) return;
      e.preventDefault();
      e.stopPropagation();
      void runUpload(resolveDropTarget(rowData(e.target as Element), cwd), e.dataTransfer);
    },
  };
  const activePath = ctx?.item.path ?? null;
  const showingSearch = searchOpen && query.trim().length > 0;

  return (
    <RemoteRows.Provider value={hostName}>
    <GripEnv.Provider value={gripEnv}>
    <DropPath.Provider value={dropPath}>
    <SyncStates.Provider value={syncStates}>
      <div className="git-panel-header file-panel-header">
        <Folder size={13} className="file-panel-folder" />
        <span className="file-panel-title" onMouseEnter={(e) => showTt(cwd, e.currentTarget)} onMouseLeave={hideTt}>{baseName(cwd, pathRules)}</span>
        <button className="git-panel-refresh" disabled={!parent} onClick={() => parent && setCwd(parent)} onMouseEnter={(e) => showTt(parent ? "Go to parent folder" : "At filesystem root", e.currentTarget)} onMouseLeave={hideTt}><ArrowUp size={12} /></button>
        <button className={`git-panel-refresh ${searchOpen ? "active" : ""}`} onClick={() => { setSearchOpen((v) => !v); if (searchOpen) setQuery(""); }} onMouseEnter={(e) => showTt("Search", e.currentTarget)} onMouseLeave={hideTt}><Search size={12} /></button>
        <button className={`git-panel-refresh ${refreshing ? "spinning" : ""}`} onClick={() => load()} onMouseEnter={(e) => showTt("Refresh", e.currentTarget)} onMouseLeave={hideTt}><RefreshCw size={11} /></button>
      </div>
      {/* Always mounted; the grid-rows trick animates it open/closed smoothly (same approach
          as the folder expand/collapse). Input is taken out of the tab order while collapsed. */}
      <div className={`file-search-wrap ${searchOpen ? "open" : ""}`}>
        <div className="file-search-inner">
          <div className="file-search-bar">
            <Search size={12} className="file-search-icon" />
            <input ref={searchInputRef} className="file-search-input" value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search this folder…" spellCheck={false} tabIndex={searchOpen ? 0 : -1} />
            {searching && <span className="file-search-spinner" />}
            {query && <button className="file-search-clear" onClick={() => { setQuery(""); searchInputRef.current?.focus(); }} aria-label="Clear"><XIcon size={12} /></button>}
          </div>
        </div>
      </div>
      <div className={`file-tree-scroll ${dropPath !== null && dropPath === cwd ? "file-tree-drop" : ""}`} {...dropHandlers}>
        {showingSearch ? (
          searching && results === null ? <div className="file-searching"><span className="file-search-spinner" /><span>Searching…</span></div>
          : results && results.length === 0 ? <div className="git-panel-empty">No matches</div>
          : (results || []).map((item) => {
              const sub = relDir(item.path, cwd, item.name);
              const onActivate = () => { if (item.is_dir) { setCwd(item.path); setSearchOpen(false); setQuery(""); } else openFile(item.path); };
              return <FileRow key={item.path} item={item} depth={0} active={activePath === item.path} subtitle={sub || undefined} onActivate={onActivate} onContext={openContext} showTt={showTtDelayed} hideTt={hideTtNow} />;
            })
        ) : (
          <>
            {roots === null && <div className="git-panel-empty">Loading…</div>}
            {roots !== null && roots.length === 0 && <div className="git-panel-empty">Empty folder</div>}
            {roots?.map((item) => <Node key={item.path} item={item} depth={0} tick={tick} host={host} activePath={activePath} onOpen={openFile} onContext={openContext} showTt={showTtDelayed} hideTt={hideTtNow} />)}
          </>
        )}
      </div>
      {dropPath !== null && <div className="file-drop-hint">{fitNotice("explorer.drop.here", { folder: baseName(dropPath, pathRules) }, 60).text}</div>}
      <TransferStrip jobs={jobs} />
      {ctx && (
        <>
          <div className="file-ctx-backdrop" onClick={() => setCtx(null)} onContextMenu={(e) => { e.preventDefault(); setCtx(null); }} />
          <div className="file-ctx-menu" style={{ left: Math.min(ctx.x, window.innerWidth - 220), top: Math.min(ctx.y, window.innerHeight - 145) }}>
            {!ctx.item.is_dir && <button className="file-ctx-item" onClick={() => { openFile(ctx.item.path); setCtx(null); }}><ExternalLink size={13} /><span>{fmt("explorer.menu.open")}</span></button>}
            <button className="file-ctx-item" onClick={() => { const item = ctx.item; setCtx(null); void runDownload(item); }}><Download size={13} /><span>{fmt("explorer.menu.downloadTo")}</span></button>
            {!host && <button className="file-ctx-item" onClick={() => { reveal(ctx.item.path); setCtx(null); }}><FolderOpen size={13} /><span>{fmt("explorer.menu.reveal")}</span></button>}
            <button className="file-ctx-item" onClick={() => { writePath(ctx.item.path); setCtx(null); }}><TerminalIcon size={13} /><span>{fmt("explorer.menu.writePath")}</span></button>
          </div>
        </>
      )}
      {clash && <ClashDialog prompt={clash} />}
      <ExplorerNotices host={host} hostName={hostName} sync={sync} extra={notices} />
    </SyncStates.Provider>
    </DropPath.Provider>
    </GripEnv.Provider>
    </RemoteRows.Provider>
  );
}

interface NoticeItem {
  id: string;
  text: string;
  full: string;
  tone: "info" | "warn" | "error";
  actions: { label: string; title?: string; run: () => void }[];
}

function sessionNotice(v: SyncView, host: string): NoticeItem | null {
  const make = (key: StringKey, tone: NoticeItem["tone"], actions: NoticeItem["actions"] = [], extra: Record<string, string | number> = {}): NoticeItem => {
    // The full text names the whole remote path; the shown one fits the notice budget.
    const { text } = fitNotice(key, { name: v.name, host, error: v.error ?? "", ...extra });
    const { full } = fitNotice(key, { name: v.remotePath, host, error: v.error ?? "", ...extra });
    return { id: v.key, text, full, tone, actions };
  };
  switch (v.state) {
    case "downloading":
      return make("explorer.open.downloading", "info", [], { pct: Math.round((v.progress ?? 0) * 100) });
    case "hashing":
    case "uploading":
      return make(v.checking ? "explorer.sync.checking" : "explorer.sync.uploading", "info");
    case "waitingHost":
      return make(v.checking ? "explorer.sync.checking" : "explorer.sync.waiting", "warn");
    case "unknownOutcome":
      return make(v.checking ? "explorer.sync.checking" : "explorer.sync.unknownOutcome", "warn");
    case "conflict":
      return v.checking ? make("explorer.sync.checking", "error") : make("explorer.sync.conflict", "error", [
        { label: fmt("explorer.sync.overwrite"), run: () => void syncBack.resolveConflict(v.key, "overwrite") },
        { label: fmt("explorer.sync.keepRemote"), title: fmt("explorer.sync.keepRemoteHint", { host }), run: () => void syncBack.resolveConflict(v.key, "keepRemote") },
      ]);
    case "failed":
      return make("explorer.sync.failed", "error", [{ label: fmt("explorer.sync.retry"), run: () => void syncBack.retry(v.key) }]);
    case "missing":
      return make("explorer.sync.missing", "error", [
        { label: fmt("explorer.sync.downloadAgain"), run: () => void syncBack.downloadAgain(v.key) },
        { label: fmt("explorer.sync.close"), run: () => void syncBack.close(v.key) },
      ]);
    default:
      return null;
  }
}

function transientNotice(n: SyncNotice, host: string): NoticeItem {
  const dismiss = { label: fmt("explorer.notice.dismiss"), run: () => syncBack.dismiss(n.id) };
  const id = `n${n.id}`;
  switch (n.kind) {
    case "uploaded":
      return { id, ...fitNotice("explorer.sync.uploaded", { name: n.name, host }), tone: "info", actions: [] };
    case "openFailed":
      return { id, ...fitNotice("explorer.open.failed", { name: n.name, error: n.error }), tone: "error", actions: [dismiss] };
    case "restored":
      return { id, ...fitNotice("explorer.sync.restored", { n: n.n, host }), tone: "warn", actions: [dismiss] };
  }
}

// The explorer's notice stack: copies of this Host's files that are syncing or need the user,
// and short-lived results. Shown in every explorer of the Host, so a copy whose explorer
// closed is still reachable.
function ExplorerNotices({ host, hostName, sync, extra }: { host?: HostId; hostName: string | null; sync: { sessions: SyncView[]; notices: SyncNotice[] }; extra: NoticeItem[] }) {
  const name = hostName ?? "";
  const items: NoticeItem[] = [];
  if (host) for (const v of sync.sessions) if (v.host === host) { const n = sessionNotice(v, name); if (n) items.push(n); }
  for (const n of sync.notices) if (n.host === host) items.push(transientNotice(n, name));
  items.push(...extra);
  if (items.length === 0) return null;
  return (
    <div className="explorer-notices" role="status">
      {items.map((n) => (
        <div key={n.id} className={`explorer-notice explorer-notice-${n.tone}`} title={n.full}>
          <span className="explorer-notice-text">{n.text}</span>
          {n.actions.length > 0 && (
            <span className="explorer-notice-actions">
              {n.actions.map((a) => <button key={a.label} className="explorer-notice-btn" title={a.title} onClick={a.run}>{a.label}</button>)}
            </span>
          )}
        </div>
      ))}
    </div>
  );
}

// An explorer transfer in progress: an upload batch or a download.
interface Job {
  id: string;
  text: string;
  pct: number;
  cancel: () => void;
}

// Info notices of transfers go away by themselves after this; errors stay until dismissed.
const NOTICE_INFO_MS = 8000;

// The transfers of this explorer, at the bottom of the panel, each with its Cancel.
function TransferStrip({ jobs }: { jobs: Job[] }) {
  if (jobs.length === 0) return null;
  return (
    <div className="file-transfer-strip" role="status">
      {jobs.map((j) => (
        <div key={j.id} className="file-transfer">
          <div className="file-transfer-row">
            <span className="file-transfer-text" title={j.text}>{j.text}</span>
            <button className="explorer-notice-btn" onClick={j.cancel}>{fmt("explorer.transfer.cancel")}</button>
          </div>
          <div className="file-transfer-bar"><div className="file-transfer-fill" style={{ width: `${Math.max(0, Math.min(100, j.pct))}%` }} /></div>
        </div>
      ))}
    </div>
  );
}

interface ClashPrompt {
  name: string;
  folder: string;
  answer: (a: ClashAnswer) => void;
}

// A dropped file's name is taken in the target folder: Replace, Keep both or Skip; "Apply to
// all" answers the rest of this batch.
function ClashDialog({ prompt }: { prompt: ClashPrompt }) {
  const [all, setAll] = useState(false);
  const answer = (choice: ClashAnswer["choice"]) => prompt.answer({ choice, applyAll: all });
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") { e.stopPropagation(); prompt.answer({ choice: "skip", applyAll: false }); } };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [prompt]);
  return (
    <>
      <div className="file-ctx-backdrop" />
      <div className="file-ctx-menu file-clash-dialog" role="dialog" aria-label={fmt("explorer.clash.title", { name: prompt.name })}>
        <div className="skip-perms-confirm-title" title={prompt.name}>{fitNotice("explorer.clash.title", { name: prompt.name }, 60).text}</div>
        <div className="file-ctx-confirm">{fitNotice("explorer.clash.body", { name: prompt.name, folder: prompt.folder }).text}</div>
        <label className="file-clash-all"><input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />{fmt("explorer.clash.applyAll")}</label>
        <div className="file-clash-actions">
          <button className="explorer-notice-btn" onClick={() => answer("replace")}>{fmt("explorer.clash.replace")}</button>
          <button className="explorer-notice-btn" onClick={() => answer("keep_both")}>{fmt("explorer.clash.keepBoth")}</button>
          <button className="explorer-notice-btn" onClick={() => answer("skip")}>{fmt("explorer.clash.skip")}</button>
        </div>
      </div>
    </>
  );
}
