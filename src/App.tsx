import { useState, useEffect, useCallback, useLayoutEffect, useMemo, useRef } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { defaultShellForPlatform, getDefaultShellId, getShellById } from "./shells";
import { Sidebar } from "./components/Sidebar";
import { TabBar } from "./components/TabBar";
import { HomeView } from "./components/HomeView";
import { TerminalTab } from "./components/TerminalTab";
import { SettingsView, type ThemeMode } from "./components/SettingsView";
import { DARK_TERM_BG, LIGHT_TERM_BG } from "./components/TerminalTab";
import { ProjectEditorDialog } from "./components/ProjectEditorDialog";
import { ProjectPicker } from "./components/ProjectPicker";
import { AgentPickerDialog } from "./components/AgentPickerDialog";
import { AGENT_IDS, AGENTS, type AgentId } from "./agents";
import type { ProjectInfo, ProjectSettings, SessionFolder, SessionInfo, Tab, Group, LayoutNode, SidebarItem, SidebarFolder } from "./types";
import { GroupView } from "./components/GroupView";
import { countLeaves, collectLeafIds, insertLeaf, removeLeaf, setRatioAt, DropZone } from "./layout";
import { useUpdateCheck } from "./hooks/useUpdateCheck";
import { UpdateDialog } from "./components/UpdateDialog";
import { hostInvoke, hostQuery } from "./hosts/hostInvoke";
import { asProjectKey, toProjectKey, encodedNameFor, keyOf, keyOfTab, lookupKey, parseProjectKey, sameKey, sessionKeyOf, sessionKeyOfTab, type ProjectKey } from "./hosts/projectKey";
import { latestGate } from "./hosts/requestGate";
import { registry } from "./hosts/registry";
import { cache } from "./hosts/cache";
import { applyFocusRemovals, applyGroupRemovals, applyTabDelta, isEmptyDelta, reconcile, restoreGroupIds, tabFromTerminal } from "./hosts/reconcile";
import { localEdits, metaSync } from "./hosts/metaSync";
import { planNewChat, planNewShell, planOpenSession, type OpenContext, type Plan } from "./hosts/sessionOps";
import { markClosing, pendingOpens, pendingUuids, remoteTerminals } from "./hosts/terminalTransport";
import { mergeRecent } from "./hosts/aggregate";
import { statusLabel, useHostsSnapshot } from "./hosts/useHosts";
import type { HostConfig, HostId, TerminalInfo } from "./hosts/types";
import { AppNotice } from "./components/AppNotice";

// Flatten sidebar items to an ordered list of project keys (folders expanded in place).
// Used to derive `savedPaths` for downstream code that doesn't care about folders. A Local
// Project's key is its bare path, so stored data needs no migration.
function flattenSidebarPaths(layout: SidebarItem[]): ProjectKey[] {
  const out: ProjectKey[] = [];
  for (const item of layout) {
    if (item.kind === "project") out.push(item.path);
    else for (const p of item.projectPaths) out.push(p);
  }
  return out;
}

function removeProjectFromLayout(layout: SidebarItem[], key: ProjectKey): SidebarItem[] {
  const out: SidebarItem[] = [];
  for (const item of layout) {
    if (item.kind === "project") {
      if (!sameKey(item.path, key)) out.push(item);
    } else {
      const kept = item.projectPaths.filter(p => !sameKey(p, key));
      if (kept.length > 0) out.push({ ...item, projectPaths: kept });
      // An empty folder is dropped entirely — no ghost folders sticking around.
    }
  }
  return out;
}

function addProjectToLayout(layout: SidebarItem[], key: ProjectKey): SidebarItem[] {
  if (flattenSidebarPaths(layout).some(p => sameKey(p, key))) return layout;
  return [...layout, { kind: "project", path: key }];
}

// Overlay that paints a drop-zone rectangle (edge of a target pane) while a tab is
// being dragged. Computed from the target pane's live bounding rect + the zone.
function DropZoneOverlay({ targetTabId, zone }: { targetTabId: string; zone: "left" | "right" | "top" | "bottom" }) {
  const el = document.querySelector(`[data-group-leaf="${CSS.escape(targetTabId)}"]`) as HTMLElement | null;
  if (!el) return null;
  const r = el.getBoundingClientRect();
  const area = (el.closest(".work-area") as HTMLElement | null)?.getBoundingClientRect();
  if (!area) return null;
  const left = r.left - area.left;
  const top = r.top - area.top;
  const fullW = r.width, fullH = r.height;
  let box: React.CSSProperties = { left, top, width: fullW, height: fullH };
  if (zone === "left")   box = { left,                    top,                    width: fullW * 0.5, height: fullH };
  if (zone === "right")  box = { left: left + fullW * 0.5, top,                    width: fullW * 0.5, height: fullH };
  if (zone === "top")    box = { left,                    top,                    width: fullW,       height: fullH * 0.5 };
  if (zone === "bottom") box = { left,                    top: top + fullH * 0.5,  width: fullW,       height: fullH * 0.5 };
  return <div className="drop-zone-preview" style={box} />;
}

export default function App() {
  // Projects per Host ("local" plus each Remote Host id); remote items carry `host`.
  const [projectsByHost, setProjectsByHost] = useState<Record<string, ProjectInfo[]>>({ local: [] });
  const allProjects = useMemo(() => {
    const remote = Object.entries(projectsByHost).filter(([k]) => k !== "local");
    if (remote.length === 0) return projectsByHost.local;
    return [...projectsByHost.local, ...remote.flatMap(([, v]) => v)];
  }, [projectsByHost]);
  const setAllProjects = useCallback((local: ProjectInfo[]) => setProjectsByHost(prev => ({ ...prev, local })), []);
  const [savedPaths, setSavedPaths] = useState<ProjectKey[]>([]);
  // Discord-style sidebar — top-level list of projects and folders-of-projects. `savedPaths`
  // is kept as a derived flat view (used by other components that just want "which projects
  // are pinned") but `sidebarLayout` is the source of truth for ordering + grouping.
  const [sidebarLayout, setSidebarLayout] = useState<SidebarItem[]>([]);
  const [projectIcons, setProjectIcons] = useState<Record<string, ProjectSettings>>({});
  const [userProjects, setUserProjects] = useState<ProjectInfo[]>([]);
  const [selectedProject, setSelectedProject] = useState<ProjectInfo | null>(null);
  const [tabs, setTabs] = useState<Tab[]>([]);
  const [activeTabId, setActiveTabId] = useState("home");
  const [recentByHost, setRecentByHost] = useState<Record<string, SessionInfo[]>>({ local: [] });
  const recentSessions = useMemo(() => {
    const remote = Object.entries(recentByHost).filter(([k]) => k !== "local");
    if (remote.length === 0) return recentByHost.local;
    return mergeRecent([recentByHost.local, ...remote.map(([, v]) => v)], 100);
  }, [recentByHost]);
  const setRecentSessions = useCallback((local: SessionInfo[]) => setRecentByHost(prev => ({ ...prev, local })), []);
  // Remote Hosts: registry snapshot (dormant and constant when no Hosts are configured).
  const hostsSnap = useHostsSnapshot();
  const [notice, setNotice] = useState<{ id: number; text: string } | null>(null);
  const showNotice = useCallback((text: string) => setNotice({ id: Date.now(), text }), []);
  const dismissNotice = useCallback(() => setNotice(null), []);
  const configuredHostsRef = useRef<HostConfig[]>([]);
  // A Remote Host's projects and recent sessions, stamped with `host` at this boundary. Served
  // from the cache while the Host is offline.
  const fetchHostData = useCallback(async (host: HostId) => {
    const [projects, sessions] = await Promise.all([
      hostInvoke<ProjectInfo[]>(host, "list_claude_projects").catch(() => null),
      hostInvoke<SessionInfo[]>(host, "get_all_recent_sessions", { limit: 100 }).catch(() => null),
    ]);
    if (!registry.isConfigured(host)) return;
    if (projects) setProjectsByHost(prev => ({ ...prev, [host]: projects.map(p => ({ ...p, host })) }));
    if (sessions) setRecentByHost(prev => ({ ...prev, [host]: sessions.map(x => ({ ...x, host })) }));
  }, []);
  // Refetch a Host's data whenever it becomes usable.
  useEffect(() => registry.onUsable(host => { fetchHostData(host); }), [fetchHostData]);
  const [projectSessions, setProjectSessions] = useState<SessionInfo[]>([]);
  // Remote project page served from the offline cache (stays true until a live fetch).
  const [projectSessionsStale, setProjectSessionsStale] = useState(false);
  const [initialLoading, setInitialLoading] = useState(true);
  const [sessionsLoading, setSessionsLoading] = useState(false);
  // Lazy polling = only fetch git status while the panel is open (a single fetch fires at
  // session start so the activity-bar icon has something to show). Eager polling re-fetches
  // every 3s while the tab is active. Default lazy: most users only need fresh git data
  // when they're actually looking at it.
  const [gitLazyPolling, setGitLazyPolling] = useState(true);
  // Show git changes as a folder tree (default). Off = flat list with a dimmed path per file.
  const [gitChangesTree, setGitChangesTree] = useState(true);
  // When on (default), a newly opened agent terminal shows the file-explorer panel immediately.
  const [fileExplorerOnStart, setFileExplorerOnStart] = useState(true);
  const [contextTreeEnabled, setContextTreeEnabled] = useState(true);
  // Both default to true: setting up the statusline hook is the meaningful gesture, the
  // toggles let the user hide either feature even with stats available.
  const [showRateLimitInSidebar, setShowRateLimitInSidebar] = useState(true);
  // Codex's twin of rate_limit_in_sidebar. Independent because the data source differs:
  // Claude's limits need the statusline hook, Codex's come straight from its rollout files
  // (so this toggle has no hook gate). The sidebar chip shows whichever agents are enabled
  // and have data; both share one popover.
  const [showRateLimitInSidebarCodex, setShowRateLimitInSidebarCodex] = useState(true);
  const [showSessionRowMetrics, setShowSessionRowMetrics] = useState(true);
  // Codex's twin of session_row_metrics — independent because the data sources differ:
  // Claude row metrics need the statusline hook, Codex reads its rollout files directly.
  const [showSessionRowMetricsCodex, setShowSessionRowMetricsCodex] = useState(true);
  // opencode's twin — its metrics come straight from opencode.db, no hook needed.
  const [showSessionRowMetricsOpencode, setShowSessionRowMetricsOpencode] = useState(true);
  // Replaces the project path in the Claude terminal header with a cost/context strip.
  // Only takes effect when the statusline hook has populated authoritative stats for the
  // session — without it there'd be nothing to show, so the header keeps the path.
  const [showTerminalHeaderStats, setShowTerminalHeaderStats] = useState(true);
  // Daily-cost chart + totals panel above the session list on the project page. Same
  // dependency on the statusline hook — the chart series comes from xshell-stats data.
  const [showProjectStatsChart, setShowProjectStatsChart] = useState(true);
  const [terminalBgColor, setTerminalBgColor] = useState("#1c1c1b");
  const [defaultTerminalFontSize, setDefaultTerminalFontSize] = useState(14);
  const [alwaysOnTop, setAlwaysOnTop] = useState(false);
  // Sets CLAUDE_CODE_NO_FLICKER=1 on every claude session so it uses the alternate-screen
  // buffer renderer. Default ON — flicker-free is what most users want; only flip if the
  // user wants scrollback-style output (or hits a renderer bug).
  const [fullscreenRendering, setFullscreenRendering] = useState(true);
  // Sets CLAUDE_CODE_FORCE_SYNC_OUTPUT=1 so claude wraps each TUI frame in DEC 2026
  // synchronized-output markers. xterm.js v5+ honors them and renders only complete
  // frames — fixes the "flying letters" residue where xterm would otherwise see
  // half-drawn intermediate frames. Default ON — strongly recommended.
  const [forceSyncOutput, setForceSyncOutput] = useState(true);
  // Use xterm.js's GPU-accelerated WebGL renderer. Default ON — it eliminates the subpixel
  // seams that show up in Claude Code's startup banner (half-block Unicode chars on the
  // DOM renderer pick up a faint horizontal line between the upper and lower halves) and
  // is generally smoother. Falls back to the DOM renderer if the host's GPU can't give us
  // a WebGL context.
  const [webglRendering, setWebglRendering] = useState(true);
  // CSS font weight applied to terminal text. 300 matches the original hardcoded value;
  // 400 reads heavier and helps compensate for the WebGL renderer's grayscale-only AA.
  const [terminalFontWeight, setTerminalFontWeight] = useState(400);
  // Spawn each restored tab's PTY on app launch instead of deferring until the user clicks the
  // tab. Default OFF — eager-init spawns every restored agent at once on launch (heavy, and
  // burns rate limits on sessions you may not open). Opt in via Settings; a persisted choice
  // overrides this default. The "Starting…" overlay covers the per-tab boot when deferred.
  const [eagerInitTabs, setEagerInitTabs] = useState(false);
  const [defaultShell, setDefaultShell] = useState<string>(getDefaultShellId());
  // Cost vs Tokens for the per-project stats panel. Global, not per-project — reflects what
  // the user cares about generally, not a trait of any one project.
  const [projectStatsView, setProjectStatsView] = useState<'cost' | 'tokens'>('cost');
  const [theme, setTheme] = useState<ThemeMode>("dark");
  const [showProjectPicker, setShowProjectPicker] = useState(false);
  const [editingProjectKey, setEditingProjectKey] = useState<ProjectKey | null>(null);
  // Which agent CLIs exist on this machine — gates every agent-choice surface (plus
  // button, dropdown group, default-agent setting). Until the probe lands we assume
  // Claude-only, which matches the app's pre-Codex behavior.
  const [installedAgents, setInstalledAgents] = useState<Record<AgentId, boolean>>(() => Object.fromEntries(AGENT_IDS.map(id => [id, id === "claude"])) as Record<AgentId, boolean>);
  // "ask" = show the agent picker dialog per new chat (only relevant with 2+ agents).
  const [defaultAgent, setDefaultAgent] = useState<"ask" | AgentId>("ask");
  // Project waiting on an agent choice — set when a new chat needs the picker dialog.
  const [agentPickerProject, setAgentPickerProject] = useState<ProjectInfo | null>(null);

  useEffect(() => {
    AGENT_IDS.forEach(id => {
      hostInvoke<{ installed: boolean }>(undefined, "detect_agent_binary", { binary: AGENTS[id].binary })
        .then(p => setInstalledAgents(prev => ({ ...prev, [id]: p.installed })))
        .catch(() => {});
    });
  }, []);
  // Update check — fetches GitHub Releases on mount; the result drives the red badge on the
  // Settings cog (Sidebar), the About page (SettingsView), and the on-start dialog.
  const updateInfo = useUpdateCheck();
  // One-time-per-version dialog — opens once per launch when GitHub has a newer release AND
  // the user hasn't already skipped that specific version. `lastSeenUpdateVersion` is loaded
  // from the store and re-written on "Skip this version".
  const [lastSeenUpdateVersion, setLastSeenUpdateVersion] = useState<string | null>(null);
  const [lastSeenLoaded, setLastSeenLoaded] = useState(false);
  const [updateDialogOpen, setUpdateDialogOpen] = useState(false);
  const [updateDialogShown, setUpdateDialogShown] = useState(false);
  const tabsRef = useRef<Tab[]>([]);
  const activeTabIdRef = useRef<string>("home");

  useEffect(() => { tabsRef.current = tabs; }, [tabs]);
  useEffect(() => { activeTabIdRef.current = activeTabId; }, [activeTabId]);

  // Suppress the WebView's native right-click menu (Back / Reload / Save / Print) app-wide —
  // it's never useful in a desktop app and collides with our own context menus. Still allowed
  // on text fields so the OS cut/copy/paste menu works there.
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (t && t.closest('input, textarea, [contenteditable]:not([contenteditable="false"])')) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", onContextMenu);
    return () => document.removeEventListener("contextmenu", onContextMenu);
  }, []);

  // ── Initial load ──────────────────────────────────────────────────
  const [tabsRestored, setTabsRestored] = useState(false);
  useEffect(() => {
    (async () => {
      try {
        const store = await load("settings.json", { defaults: {}, autoSave: true });
        const [paths, icons, savedTabs, savedGroups, gitLazy, bgColor, aot, shell, ctxEnabled, defFont, gitTree, fileExpOnStart, storedLayout, rlSidebar, rowMetrics, storedTheme, fsRender, termHeaderStats, projectStatsChart, statsView, syncOut, eagerInit, webgl, fontWeight, defAgent, rowMetricsCodex, rlSidebarCodex, rowMetricsOpencode, storedHosts] = await Promise.all([
          store.get<string[]>("project_paths"),
          store.get<Record<string, ProjectSettings>>("project_icons"),
          store.get<Tab[]>("open_tabs"),
          store.get<Group[]>("open_groups"),
          store.get<boolean>("git_lazy_polling"),
          store.get<string>("terminal_bg_color"),
          store.get<boolean>("always_on_top"),
          store.get<string>("default_shell"),
          store.get<boolean>("context_tree_enabled"),
          store.get<number>("default_terminal_font_size"),
          store.get<boolean>("git_changes_tree"),
          store.get<boolean>("file_explorer_on_start"),
          store.get<SidebarItem[]>("sidebar_layout"),
          store.get<boolean>("rate_limit_in_sidebar"),
          store.get<boolean>("session_row_metrics"),
          store.get<ThemeMode>("theme"),
          store.get<boolean>("fullscreen_rendering_enabled"),
          store.get<boolean>("terminal_header_stats"),
          store.get<boolean>("project_stats_chart"),
          store.get<'cost' | 'tokens'>("project_stats_view"),
          store.get<boolean>("force_sync_output_enabled"),
          store.get<boolean>("eager_init_tabs"),
          store.get<boolean>("webgl_rendering_enabled"),
          store.get<number>("terminal_font_weight"),
          store.get<string>("default_agent"),
          store.get<boolean>("session_row_metrics_codex"),
          store.get<boolean>("rate_limit_in_sidebar_codex"),
          store.get<boolean>("session_row_metrics_opencode"),
          store.get<HostConfig[]>("hosts"),
        ]);
        // Remote Hosts: start the registry (a no-op with none configured) and load the
        // offline cache so their last known Terminals show as Tabs right away.
        const hostConfigs: HostConfig[] = Array.isArray(storedHosts) ? storedHosts : [];
        if (hostConfigs.length > 0) {
          await cache.load();
          registry.init(hostConfigs).catch(() => {});
        }
        configuredHostsRef.current = hostConfigs;
        // Layout: prefer the explicit `sidebar_layout` if present; otherwise migrate
        // from the flat `project_paths` list by wrapping each path in a project item.
        let layout: SidebarItem[] = [];
        if (Array.isArray(storedLayout) && storedLayout.length > 0) {
          layout = storedLayout;
        } else if (paths && paths.length > 0) {
          layout = paths.map(p => ({ kind: "project" as const, path: asProjectKey(p) }));
        }
        setSidebarLayout(layout);
        // Derive the flat paths list from the layout so downstream code stays happy.
        const derivedPaths = flattenSidebarPaths(layout);
        if (derivedPaths.length) setSavedPaths(derivedPaths);
        else if (paths) setSavedPaths(paths.map(asProjectKey));
        if (icons) setProjectIcons(icons);
        if (typeof gitLazy === "boolean") setGitLazyPolling(gitLazy);
        if (typeof bgColor === "string") setTerminalBgColor(bgColor);
        if (typeof aot === "boolean") setAlwaysOnTop(aot);
        if (typeof shell === "string") setDefaultShell(shell);
        if (typeof ctxEnabled === "boolean") setContextTreeEnabled(ctxEnabled);
        if (typeof defFont === "number" && defFont >= 8 && defFont <= 32) setDefaultTerminalFontSize(defFont);
        if (typeof rlSidebar === "boolean") setShowRateLimitInSidebar(rlSidebar);
        if (typeof rowMetrics === "boolean") setShowSessionRowMetrics(rowMetrics);
        if (typeof gitTree === "boolean") setGitChangesTree(gitTree);
        if (typeof fileExpOnStart === "boolean") setFileExplorerOnStart(fileExpOnStart);
        if (typeof fsRender === "boolean") setFullscreenRendering(fsRender);
        if (typeof syncOut === "boolean") setForceSyncOutput(syncOut);
        if (typeof eagerInit === "boolean") setEagerInitTabs(eagerInit);
        if (typeof webgl === "boolean") setWebglRendering(webgl);
        if (typeof fontWeight === "number" && fontWeight >= 100 && fontWeight <= 700) setTerminalFontWeight(fontWeight);
        if (typeof termHeaderStats === "boolean") setShowTerminalHeaderStats(termHeaderStats);
        if (typeof projectStatsChart === "boolean") setShowProjectStatsChart(projectStatsChart);
        if (storedTheme === "light" || storedTheme === "dark") setTheme(storedTheme);
        if (statsView === "cost" || statsView === "tokens") setProjectStatsView(statsView);
        if (defAgent === "ask" || (typeof defAgent === "string" && (AGENT_IDS as string[]).includes(defAgent))) setDefaultAgent(defAgent as "ask" | AgentId);
        if (typeof rowMetricsCodex === "boolean") setShowSessionRowMetricsCodex(rowMetricsCodex);
        if (typeof rlSidebarCodex === "boolean") setShowRateLimitInSidebarCodex(rlSidebarCodex);
        if (typeof rowMetricsOpencode === "boolean") setShowSessionRowMetricsOpencode(rowMetricsOpencode);
        // Restore only tabs that have a real sessionId (not abandoned "New Chat" tabs). Remote
        // Tabs are not in open_tabs: they come from the cached `terminals` list per Host.
        const cachedRemote: Tab[] = hostConfigs.flatMap(h => (cache.terminals(h.id) ?? []).slice().sort((a, b) => a.createdAtMs - b.createdAtMs).map(info => tabFromTerminal(h.id, info)));
        if (savedTabs?.length || cachedRemote.length) {
          const restorable = [...(savedTabs ?? []).filter(t => t.sessionId && t.projectPath && !t.host), ...cachedRemote];
          if (restorable.length) {
            // First, filter groups: keep only those whose leaves are all restorable.
            const restoredIds = new Set(restorable.map(t => t.id));
            const keptGroups: Group[] = [];
            if (savedGroups?.length) {
              for (const g of savedGroups) {
                const leaves = collectLeafIds(g.layout);
                if (leaves.length >= 2 && leaves.every(id => restoredIds.has(id))) keptGroups.push(g);
              }
            }
            const validGroupIds = new Set(keptGroups.map(g => g.id));
            // Then, scrub any orphaned groupId off a tab — a leftover from an earlier bug
            // where tabs kept a groupId pointing at a group that no longer exists.
            // Cached remote tabs get their groupId back from the kept layouts (amendment 22).
            const scrubbed = restoreGroupIds(restorable, keptGroups).map(t => (t.groupId && !validGroupIds.has(t.groupId)) ? { ...t, groupId: undefined } : t);
            setTabs(scrubbed);
            setGroups(keptGroups);
            let maxN = 0;
            for (const g of keptGroups) {
              const m = /^Group\s+(\d+)$/.exec(g.name);
              if (m) maxN = Math.max(maxN, parseInt(m[1], 10));
            }
            groupCounterRef.current = maxN + 1;
          }
        }
      } catch (_) {}
      setTabsRestored(true);
      for (const h of configuredHostsRef.current) fetchHostData(h.id);
      const [projects, sessions] = await Promise.all([
        hostInvoke<ProjectInfo[]>(undefined, "list_claude_projects").catch(() => [] as ProjectInfo[]),
        hostInvoke<SessionInfo[]>(undefined, "get_all_recent_sessions", { limit: 100 }).catch(() => [] as SessionInfo[]),
      ]);
      setAllProjects(projects);
      setRecentSessions(sessions);
      setInitialLoading(false);
    })();
  }, []);

  // ── Load the last skipped-update version from the store ───────────
  useEffect(() => {
    (async () => {
      try {
        const store = await load("settings.json", { defaults: {}, autoSave: true });
        const v = await store.get<string>("last_seen_update_version");
        if (typeof v === "string") setLastSeenUpdateVersion(v);
      } catch (_) {}
      setLastSeenLoaded(true);
    })();
  }, []);

  // Open the update dialog once per launch, only if the user hasn't already skipped this
  // exact version. `updateDialogShown` ensures it never re-opens within the same session
  // even if the hook re-renders.
  useEffect(() => {
    if (!lastSeenLoaded || updateDialogShown) return;
    if (updateInfo.loading || updateInfo.error) return;
    if (!updateInfo.updateAvailable || !updateInfo.latestVersion) return;
    if (lastSeenUpdateVersion === updateInfo.latestVersion) return;
    setUpdateDialogOpen(true);
    setUpdateDialogShown(true);
  }, [lastSeenLoaded, updateDialogShown, updateInfo.loading, updateInfo.error, updateInfo.updateAvailable, updateInfo.latestVersion, lastSeenUpdateVersion]);

  // Any close path through the dialog runs through here. Always persists `last_seen_update_version`
  // so the dialog won't fire again until GitHub ships a NEWER tag — the badge + About dot are
  // unaffected and stay until the bundled version actually catches up.
  const dismissUpdateDialog = useCallback(async () => {
    const v = updateInfo.latestVersion;
    setUpdateDialogOpen(false);
    if (!v) return;
    setLastSeenUpdateVersion(v);
    try {
      const store = await load("settings.json", { defaults: {}, autoSave: true });
      await store.set("last_seen_update_version", v);
    } catch (_) {}
  }, [updateInfo.latestVersion]);

  // ── Persist tabs whenever they change (after initial restore) ─────
  useEffect(() => {
    if (!tabsRestored) return;
    (async () => {
      try {
        const store = await load("settings.json", { defaults: {}, autoSave: true });
        // Remote Tabs are mirrored from their Daemon, never persisted here.
        await store.set("open_tabs", tabs.some(t => t.host) ? tabs.filter(t => !t.host) : tabs);
      } catch (_) {}
    })();
  }, [tabs, tabsRestored]);

  // ── Derive user projects ──────────────────────────────────────────
  useEffect(() => {
    setUserProjects(savedPaths.map(key => {
      const found = allProjects.find(p => sameKey(keyOf(p), key));
      if (found) return found;
      const { host, path } = parseProjectKey(key);
      const name = path.split(/[\\/]/).filter(Boolean).pop() || path;
      const fallback: ProjectInfo = { name, path, encoded_name: "", session_count: 0, last_active: "" };
      return host ? { ...fallback, host } : fallback;
    }).filter(p => !p.host || hostsSnap.configs.some(c => c.id === p.host))); // keys of unknown Hosts are skipped
  }, [savedPaths, allProjects, hostsSnap.configs]);

  // ── Tab title sync: lightweight poll only when terminals are open ──
  useEffect(() => {
    if (tabs.length === 0) return;

    const syncTitles = async () => {
      // Distinct projects across open tabs, by key (original-cased path: encoding is
      // case-sensitive). A Project is (Host, path), so the same path on two Hosts is polled twice.
      const projectsByKey = new Map<string, { host?: ProjectInfo["host"]; path: string }>();
      for (const t of tabs) {
        const k = keyOfTab(t);
        // Later tabs overwrite earlier ones (same casing rule as before Remote Hosts).
        if (k) projectsByKey.set(lookupKey(k), { host: t.host, path: t.projectPath! });
      }
      const projectMap = new Map<string, ProjectInfo>();
      for (const p of allProjects) projectMap.set(lookupKey(keyOf(p)), p);

      for (const [pp, { host, path: origPath }] of projectsByKey) {
        // Prefer Claude's recorded encoded name; otherwise mirror the Rust encoding so the
        // poll also reaches Codex/Cursor-only projects (which carry no Claude encoded_name).
        const encodedName = encodedNameFor({ encoded_name: projectMap.get(pp)?.encoded_name, path: origPath });
        if (!encodedName) continue;
        try {
          const sessions = await hostInvoke<SessionInfo[]>(host, "get_sessions", { encodedName });
          setTabs(prev => {
            let changed = false;
            // Sessions already linked to an open tab — an unlinked tab must not claim them.
            // Host-qualified, so a remote session never claims a local id (amendment 19).
            const claimed = new Set(prev.map(sessionKeyOfTab).filter(Boolean) as string[]);
            const next = prev.map(tab => {
              const tk = keyOfTab(tab);
              if (!tk || lookupKey(tk) !== pp) return tab;
              // Link an unlinked new-chat tab (Codex — which has no pre-created id — or a Cursor
              // tab whose create-chat fell back) to its freshly-created session: newest unclaimed
              // session of the same agent that appeared after the tab opened and already has a
              // real title (not the bare "Session <id>" fallback), so we rename straight to the
              // meaningful name instead of flashing an intermediate one.
              if (!tab.sessionId && tab.agent && tab.agent !== "claude") {
                const candidate = sessions
                  .filter(s => s.agent === tab.agent && !claimed.has(sessionKeyOf({ host, id: s.id })) && !s.title.startsWith("Session ") && new Date(s.timestamp).getTime() >= (tab.createdAt ?? 0))
                  .sort((a, b) => b.timestamp.localeCompare(a.timestamp))[0];
                if (candidate) { claimed.add(sessionKeyOf({ host, id: candidate.id })); changed = true; return { ...tab, sessionId: candidate.id, title: candidate.title }; }
                return tab;
              }
              // Linked tab: keep its title in sync — picks up `/rename`, ai-title, first-prompt alike.
              if (!tab.sessionId) return tab;
              const match = sessions.find(s => s.id === tab.sessionId);
              if (match && match.title !== tab.title) { changed = true; return { ...tab, title: match.title }; }
              return tab;
            });
            if (!changed) return prev;
            // Remote tabs: a link or rename is a local edit → pushed to the Terminal's meta.
            for (const e of localEdits(prev, next)) metaSync.markDirty(e.tabId, e.field, e.value);
            return next;
          });
        } catch (_) {}
      }
    };

    const interval = setInterval(syncTitles, 5000);
    return () => clearInterval(interval);
  }, [tabs.length, allProjects]); // Only re-setup when tab count or projects change

  // ── Persistence ───────────────────────────────────────────────────
  const persistPaths = useCallback(async (paths: ProjectKey[]) => {
    setSavedPaths(paths);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("project_paths", paths); } catch (_) {}
  }, []);

  // Central sidebar-layout mutator. Also refreshes the derived `savedPaths` and persists both
  // so old code paths (which still consume `savedPaths`) keep working.
  const persistSidebarLayout = useCallback(async (layout: SidebarItem[]) => {
    setSidebarLayout(layout);
    const paths = flattenSidebarPaths(layout);
    setSavedPaths(paths);
    try {
      const store = await load("settings.json", { defaults: {}, autoSave: true });
      await store.set("sidebar_layout", layout);
      await store.set("project_paths", paths);
    } catch (_) {}
  }, []);

  const persistIcons = useCallback(async (icons: Record<string, ProjectSettings>) => {
    setProjectIcons(icons);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("project_icons", icons); } catch (_) {}
  }, []);

  const persistGitLazyPolling = useCallback(async (enabled: boolean) => {
    setGitLazyPolling(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("git_lazy_polling", enabled); } catch (_) {}
  }, []);

  const persistGitChangesTree = useCallback(async (enabled: boolean) => {
    setGitChangesTree(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("git_changes_tree", enabled); } catch (_) {}
  }, []);

  const persistFileExplorerOnStart = useCallback(async (enabled: boolean) => {
    setFileExplorerOnStart(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("file_explorer_on_start", enabled); } catch (_) {}
  }, []);

  const persistContextTreeEnabled = useCallback(async (enabled: boolean) => {
    setContextTreeEnabled(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("context_tree_enabled", enabled); } catch (_) {}
  }, []);

  const persistShowRateLimitInSidebar = useCallback(async (enabled: boolean) => {
    setShowRateLimitInSidebar(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("rate_limit_in_sidebar", enabled); } catch (_) {}
  }, []);

  const persistShowRateLimitInSidebarCodex = useCallback(async (enabled: boolean) => {
    setShowRateLimitInSidebarCodex(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("rate_limit_in_sidebar_codex", enabled); } catch (_) {}
  }, []);

  const persistShowSessionRowMetrics = useCallback(async (enabled: boolean) => {
    setShowSessionRowMetrics(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("session_row_metrics", enabled); } catch (_) {}
  }, []);

  const persistShowSessionRowMetricsCodex = useCallback(async (enabled: boolean) => {
    setShowSessionRowMetricsCodex(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("session_row_metrics_codex", enabled); } catch (_) {}
  }, []);

  const persistShowSessionRowMetricsOpencode = useCallback(async (enabled: boolean) => {
    setShowSessionRowMetricsOpencode(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("session_row_metrics_opencode", enabled); } catch (_) {}
  }, []);

  const persistShowTerminalHeaderStats = useCallback(async (enabled: boolean) => {
    setShowTerminalHeaderStats(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("terminal_header_stats", enabled); } catch (_) {}
  }, []);

  const persistShowProjectStatsChart = useCallback(async (enabled: boolean) => {
    setShowProjectStatsChart(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("project_stats_chart", enabled); } catch (_) {}
  }, []);

  const persistDefaultTerminalFontSize = useCallback(async (size: number) => {
    const clamped = Math.max(8, Math.min(32, Math.round(size)));
    setDefaultTerminalFontSize(clamped);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("default_terminal_font_size", clamped); } catch (_) {}
  }, []);

  const persistTerminalBgColor = useCallback(async (color: string) => {
    setTerminalBgColor(color);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("terminal_bg_color", color); } catch (_) {}
  }, []);

  const persistAlwaysOnTop = useCallback(async (value: boolean) => {
    setAlwaysOnTop(value);
    try { await getCurrentWindow().setAlwaysOnTop(value); } catch (_) {}
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("always_on_top", value); } catch (_) {}
  }, []);

  const persistDefaultShell = useCallback(async (shellId: string) => {
    setDefaultShell(shellId);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("default_shell", shellId); } catch (_) {}
  }, []);

  const persistFullscreenRendering = useCallback(async (enabled: boolean) => {
    setFullscreenRendering(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("fullscreen_rendering_enabled", enabled); } catch (_) {}
  }, []);

  const persistProjectStatsView = useCallback(async (view: 'cost' | 'tokens') => {
    setProjectStatsView(view);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("project_stats_view", view); } catch (_) {}
  }, []);

  const persistForceSyncOutput = useCallback(async (enabled: boolean) => {
    setForceSyncOutput(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("force_sync_output_enabled", enabled); } catch (_) {}
  }, []);

  const persistEagerInitTabs = useCallback(async (enabled: boolean) => {
    setEagerInitTabs(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("eager_init_tabs", enabled); } catch (_) {}
  }, []);

  const persistWebglRendering = useCallback(async (enabled: boolean) => {
    setWebglRendering(enabled);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("webgl_rendering_enabled", enabled); } catch (_) {}
  }, []);

  const persistTerminalFontWeight = useCallback(async (weight: number) => {
    setTerminalFontWeight(weight);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("terminal_font_weight", weight); } catch (_) {}
  }, []);

  // Apply synchronously alongside the React state change so the next paint already has
  // the new tokens — avoids a flash and any useEffect-timing oddities in the WebView.
  const persistTheme = useCallback(async (next: ThemeMode) => {
    setTheme(next);
    document.documentElement.dataset.theme = next;
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("theme", next); } catch (_) {}
  }, []);

  const persistDefaultAgent = useCallback(async (next: "ask" | AgentId) => {
    setDefaultAgent(next);
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("default_agent", next); } catch (_) {}
  }, []);

  // Safety net: keep the attribute in sync with state on every change (covers the initial
  // restore from settings.json, where setTheme is called outside persistTheme).
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  // When the theme flips, slide the terminal bg setting from the previous theme's default
  // to the new theme's default — so the Settings color picker shows the right shade and
  // the saved value matches what's actually rendered. Custom colors stay put. Also fires
  // on first load: if the user originally saved #1c1c1b in dark and then picked Light,
  // this normalizes them to #faf9f5 once the stored theme is restored.
  useEffect(() => {
    const newDefault = theme === "light" ? LIGHT_TERM_BG : DARK_TERM_BG;
    const oldDefault = theme === "light" ? DARK_TERM_BG : LIGHT_TERM_BG;
    if (terminalBgColor.toLowerCase() === oldDefault) persistTerminalBgColor(newDefault);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [theme]);

  // Called by TerminalTab when a branched session is detected. Updates only the tab's
  // metadata — the PTY is already attached to the new sessionId's JSONL, so nothing else
  // needs to change.
  const handleSwitchTabToBranch = useCallback((tabId: string, newSessionId: string, newTitle: string) => {
    setTabs(prev => {
      const next = prev.map(t => t.id === tabId ? { ...t, sessionId: newSessionId, title: newTitle } : t);
      for (const e of localEdits(prev, next)) metaSync.markDirty(e.tabId, e.field, e.value);
      return next;
    });
  }, []);

  // Apply always-on-top on startup once the value has been restored from disk.
  useEffect(() => { getCurrentWindow().setAlwaysOnTop(alwaysOnTop).catch(() => {}); }, [alwaysOnTop]);


  // ── Navigation: fresh load on every navigate ──────────────────────
  // Responses for a project that is no longer selected are dropped (amendment 23).
  const projectSessionsGate = useRef(latestGate());
  const handleSelectProject = useCallback(async (project: ProjectInfo) => {
    setSelectedProject(project);
    setActiveTabId("home");
    // Prefer Claude's recorded encoded name; otherwise mirror the Rust encoding so projects
    // only ever used by Codex/Cursor/opencode (no ~/.claude entry) still list their sessions.
    const encodedName = encodedNameFor(project);
    const token = projectSessionsGate.current.begin(keyOf(project));
    if (!encodedName) { setProjectSessions([]); return; }
    setSessionsLoading(true);
    let next: SessionInfo[];
    let stale = false;
    try {
      const r = await hostQuery<SessionInfo[]>(project.host, "get_sessions", { encodedName });
      next = r.value;
      stale = r.stale;
    } catch (_) { next = []; }
    // Stamp the Host at the fetch boundary so every consumer knows where a session lives.
    if (project.host) next = next.map(x => ({ ...x, host: project.host }));
    if (!projectSessionsGate.current.isCurrent(token)) return;
    setProjectSessions(next);
    setProjectSessionsStale(stale);
    setSessionsLoading(false);
  }, []);

  const handleGoHome = useCallback(async () => {
    setSelectedProject(null);
    setActiveTabId("home");
    setSessionsLoading(true);
    const [sessions, projects] = await Promise.all([
      hostInvoke<SessionInfo[]>(undefined, "get_all_recent_sessions", { limit: 100 }).catch(() => [] as SessionInfo[]),
      hostInvoke<ProjectInfo[]>(undefined, "list_claude_projects").catch(() => projectsByHost.local),
    ]);
    setRecentSessions(sessions);
    setAllProjects(projects);
    setSessionsLoading(false);
    for (const h of registry.getSnapshot().configs) fetchHostData(h.id);
  }, [projectsByHost.local, fetchHostData]);

  // ── Project management ────────────────────────────────────────────
  const handleToggleProject = useCallback(async (key: ProjectKey) => {
    const exists = flattenSidebarPaths(sidebarLayout).some(p => sameKey(p, key));
    const next = exists ? removeProjectFromLayout(sidebarLayout, key) : addProjectToLayout(sidebarLayout, key);
    await persistSidebarLayout(next);
    if (exists && selectedProject && sameKey(keyOf(selectedProject), key)) setSelectedProject(null);
  }, [sidebarLayout, persistSidebarLayout, selectedProject]);

  const handleRemoveProject = useCallback(async (key: ProjectKey) => {
    await persistSidebarLayout(removeProjectFromLayout(sidebarLayout, key));
    if (selectedProject && sameKey(keyOf(selectedProject), key)) { setSelectedProject(null); setActiveTabId("home"); }
  }, [sidebarLayout, persistSidebarLayout, selectedProject]);

  const handleSaveProjectSettings = useCallback(async (projectKey: ProjectKey, next: ProjectSettings) => {
    const key = lookupKey(projectKey);
    const existing = projectIcons[key] || {};
    // Editor only touches icon + color + customName; preserve folders that already exist.
    const entry: ProjectSettings = { ...existing, icon: next.icon, color: next.color, customName: next.customName };
    const merged: Record<string, ProjectSettings> = { ...projectIcons, [key]: entry };
    if (!entry.icon && !entry.color && !entry.customName && (!entry.folders || entry.folders.length === 0)) delete merged[key];
    await persistIcons(merged);
  }, [projectIcons, persistIcons]);

  const handleSaveFolders = useCallback(async (projectKey: ProjectKey, folders: SessionFolder[]) => {
    const key = lookupKey(projectKey);
    const existing = projectIcons[key] || {};
    const entry: ProjectSettings = { ...existing, folders: folders.length > 0 ? folders : undefined };
    const merged: Record<string, ProjectSettings> = { ...projectIcons, [key]: entry };
    if (!entry.icon && !entry.customName && (!entry.folders || entry.folders.length === 0)) delete merged[key];
    await persistIcons(merged);
  }, [projectIcons, persistIcons]);

  const handleBrowseFolder = useCallback(async () => {
    try {
      const selected = await open({ directory: true, multiple: false, title: "Select project folder" });
      // Browse is Local only: the picked folder is on this computer.
      if (selected && typeof selected === "string" && !flattenSidebarPaths(sidebarLayout).some(p => sameKey(p, selected))) {
        await persistSidebarLayout(addProjectToLayout(sidebarLayout, asProjectKey(selected)));
        setAllProjects(await hostInvoke<ProjectInfo[]>(undefined, "list_claude_projects"));
      }
    } catch (_) {}
  }, [sidebarLayout, persistSidebarLayout]);

  // ── Settings → Hosts ──────────────────────────────────────────────
  // Persist the `hosts` key, then reconfigure the registry (starts it on the first Host).
  const persistHosts = useCallback(async (list: HostConfig[]) => {
    const added = list.filter(h => !registry.isConfigured(h.id));
    configuredHostsRef.current = list;
    // The registry drops removed Hosts synchronously, so no late event re-adds their Tabs.
    const configured = registry.configure(list).catch(() => {});
    if (list.length > 0) cache.load().catch(() => {});
    try { const store = await load("settings.json", { defaults: {}, autoSave: true }); await store.set("hosts", list); } catch (_) {}
    await configured;
    for (const h of added) fetchHostData(h.id);
  }, [fetchHostData]);

  // Remove a Host: its Tabs go away here without a close intent (they detach; the Terminals
  // keep running on the Host), its projects are unpinned and its cache is dropped.
  const handleRemoveHost = useCallback(async (id: HostId) => {
    const saved = persistHosts(registry.getSnapshot().configs.filter(c => c.id !== id));
    const gone = tabsRef.current.filter(t => t.host === id).map(t => t.id);
    for (const [uuid, p] of pendingOpens) if (p.host === id) pendingOpens.delete(uuid);
    for (const tid of gone) metaSync.forget(tid);
    if (gone.length) {
      setTabs(prev => prev.filter(t => t.host !== id));
      setGroups(prev => applyGroupRemovals(prev, gone));
      setActiveLeafByGroup(prev => applyFocusRemovals(prev, applyGroupRemovals(groupsRef.current, gone), gone));
      if (gone.includes(activeTabIdRef.current)) setActiveTabId("home");
    }
    const layout = sidebarLayout
      .map((item): SidebarItem => item.kind === "folder" ? { ...item, projectPaths: item.projectPaths.filter(k => parseProjectKey(k).host !== id) } : item)
      .filter(item => item.kind === "folder" ? item.projectPaths.length > 0 : parseProjectKey(item.path).host !== id);
    if (layout.length !== sidebarLayout.length || layout.some((it, i) => it !== sidebarLayout[i])) await persistSidebarLayout(layout);
    if (selectedProject?.host === id) { setSelectedProject(null); setActiveTabId("home"); }
    setProjectsByHost(prev => { const n = { ...prev }; delete n[id]; return n; });
    setRecentByHost(prev => { const n = { ...prev }; delete n[id]; return n; });
    delete appliedLiveRef.current[id];
    cache.dropHost(id);
    await saved;
  }, [sidebarLayout, persistSidebarLayout, selectedProject, persistHosts]);

  // Pin a folder on a Remote Host by path; validated on the Host with list_dir.
  const handleAddRemotePath = useCallback(async (host: HostId, path: string): Promise<"ok" | "notFound" | "offline"> => {
    if (!registry.isUsable(host)) return "offline";
    try { await hostInvoke(host, "list_dir", { path }); } catch (_) { return registry.isUsable(host) ? "notFound" : "offline"; }
    await persistSidebarLayout(addProjectToLayout(sidebarLayout, toProjectKey(host, path)));
    return "ok";
  }, [sidebarLayout, persistSidebarLayout]);

  // ── Tab management ────────────────────────────────────────────────
  // Context for the pure Terminal-start planners (src/hosts/sessionOps.ts).
  const openContext = useCallback((): OpenContext => ({
    now: Date.now(),
    uuid: () => crypto.randomUUID(),
    fullscreenRendering,
    forceSyncOutput,
    isUsable: (h) => registry.isUsable(h),
    isConfigured: (h) => registry.isConfigured(h),
    status: (h) => registry.getStatus(h),
    hostName: (h) => registry.hostName(h),
    statusLabel,
  }), [fullscreenRendering, forceSyncOutput]);

  // Applies a plan: refusal → notice; create → add the tab (and register a remote open).
  const applyPlan = useCallback((plan: Plan, activate: boolean): Tab | null => {
    if (plan.kind === "refuse") { showNotice(plan.notice); return null; }
    if (plan.kind === "focus") return plan.tab;
    if (plan.pending) pendingOpens.set(plan.pending.uuid, plan.pending.open);
    setTabs(prev => [...prev, plan.tab]);
    if (activate) setActiveTabId(plan.tab.id);
    return plan.tab;
  }, [showNotice]);

  // Amendment 18: one host-aware open path for every session-open entry point.
  const openSession = useCallback((session: SessionInfo, project: ProjectInfo | undefined, opts: { background: boolean }) => {
    const plan = planOpenSession(session, project, tabs, openContext());
    if (plan.kind === "focus") {
      if (opts.background) return;
      const existingTab = plan.tab;
      if (existingTab.groupId) {
        // Tab lives inside a group — surface that group and focus the matching pane.
        setActiveTabId(existingTab.groupId);
        setActiveLeafByGroup(prev => ({ ...prev, [existingTab.groupId!]: existingTab.id }));
      } else {
        setActiveTabId(existingTab.id);
      }
      return;
    }
    applyPlan(plan, !opts.background);
  }, [tabs, openContext, applyPlan]);

  const handleOpenSession = useCallback((session: SessionInfo, project?: ProjectInfo) => openSession(session, project, { background: false }), [openSession]);
  // Add as tab without switching to it — stays on current view.
  const handleOpenSessionBackground = useCallback((session: SessionInfo, project?: ProjectInfo) => openSession(session, project, { background: true }), [openSession]);

  // Which agent CLIs exist on a Host: this machine's probe for Local, the registry's for a
  // Remote Host (never the Desktop's).
  const installedAgentsFor = useCallback((host?: HostId): Record<AgentId, boolean> => {
    if (!host) return installedAgents;
    return hostsSnap.agents[host] ?? (Object.fromEntries(AGENT_IDS.map(a => [a, false])) as Record<AgentId, boolean>);
  }, [installedAgents, hostsSnap]);

  const handleNewChat = useCallback((project: ProjectInfo, agent?: AgentId) => {
    // Resolve which agent hosts the chat: explicit pick > single installed agent > the
    // user's default. With multiple agents and no default ("ask"), open the picker dialog
    // and re-enter with the chosen agent. Single-agent machines never see any of this.
    if (!agent) {
      const agents = installedAgentsFor(project.host);
      const installed = AGENT_IDS.filter(a => agents[a]);
      if (installed.length > 1) {
        if (defaultAgent !== "ask" && installed.includes(defaultAgent)) agent = defaultAgent;
        else if (defaultAgent !== "ask" && !project.host) agent = defaultAgent;
        else { setAgentPickerProject(project); return; }
      } else if (project.host && installed.length === 0) {
        // A Remote Host with no detected agent CLI: the picker explains it.
        if (!registry.isUsable(project.host)) { applyPlan(planNewChat(project, "claude", openContext()), true); return; }
        setAgentPickerProject(project);
        return;
      } else {
        agent = installed[0] ?? "claude";
      }
    }
    // Claude: a pre-allocated UUID passed via `--session-id` (known JSONL name from the start,
    // and Claude's ai-title still fires). Codex and Cursor can't pre-assign a session id, so
    // they spawn bare and the title-sync links the tab once a session with a real title appears.
    applyPlan(planNewChat(project, agent, openContext()), true);
  }, [installedAgentsFor, defaultAgent, applyPlan, openContext]);

  // Open a raw shell tab (no Claude wrapping) — disposable by design, not persisted across restart.
  // project === null → shell spawned in the user's home directory.
  const handleNewShell = useCallback((project: ProjectInfo | null, shellId: string, shellName: string) => {
    applyPlan(planNewShell(project, shellId, shellName, openContext()), true);
  }, [applyPlan, openContext]);

  const [closingTabIds, setClosingTabIds] = useState<Set<string>>(new Set());

  const handleCloseTab = useCallback((id: string) => {
    // Group close: drop the group entry + all tabs that belong to it.
    const group = groupsRef.current.find(g => g.id === id);
    if (group) {
      const memberIds = collectLeafIds(group.layout);
      // Close intent first: remote Terminals end for every Desktop (not just detach).
      markClosing(tabsRef.current.filter(t => memberIds.includes(t.id)));
      if (activeTabId === id) setActiveTabId("home");
      setGroups(prev => prev.filter(g => g.id !== id));
      setTabs(prev => prev.filter(t => !memberIds.includes(t.id)));
      return;
    }
    // Standalone tab close (existing animated path).
    markClosing(tabsRef.current.filter(t => t.id === id));
    setClosingTabIds(prev => new Set(prev).add(id));
    if (activeTabId === id) {
      const idx = tabsRef.current.findIndex(t => t.id === id);
      const remaining = tabsRef.current.filter(t => t.id !== id);
      setActiveTabId(remaining.length > 0 ? remaining[Math.min(idx, remaining.length - 1)]?.id || "home" : "home");
    }
    setTimeout(() => {
      setTabs(prev => prev.filter(t => t.id !== id));
      setClosingTabIds(prev => { const next = new Set(prev); next.delete(id); return next; });
    }, 180);
  }, [activeTabId]);

  const handleReorderProjects = useCallback(async (newPaths: ProjectKey[]) => {
    await persistPaths(newPaths);
  }, [persistPaths]);

  void handleReorderProjects; // kept for any legacy callers; new Sidebar uses onLayoutChange.

  const handleReorderTabs = useCallback((newTabs: Tab[]) => {
    setTabs(newTabs);
  }, []);

  const [hoveredProjectKey, setHoveredProjectKey] = useState<ProjectKey | null>(null);

  // Active terminal-tab count per project (used for sidebar badges), by lookupKey.
  const activeCountByProject = new Map<string, number>();
  for (const t of tabs) {
    const tk = keyOfTab(t);
    if (tk) {
      const key = lookupKey(tk);
      activeCountByProject.set(key, (activeCountByProject.get(key) || 0) + 1);
    }
  }
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const showSettings = activeTabId === "settings";
  const activeTab = tabs.find(t => t.id === activeTabId);
  const activeTabProjectPath = activeTab?.projectPath || null;
  const activeTabProjectKey = activeTab ? keyOfTab(activeTab) : null;

  // ── Groups (multi-pane split view) ────────────────────────────
  // A Group bundles up to 8 tabs into one "entry" in the tab bar, displaying them
  // in a binary-tree split layout. A tab is either standalone OR inside one group.
  const MAX_GROUP_LEAVES = 8;
  const [groups, setGroups] = useState<Group[]>([]);
  const showHome = !showSettings && !tabs.find(t => t.id === activeTabId) && !groups.find(g => g.id === activeTabId);

  // Persist groups whenever they change (after initial restore, same pattern as tabs).
  useEffect(() => {
    if (!tabsRestored) return;
    (async () => {
      try {
        const store = await load("settings.json", { defaults: {}, autoSave: true });
        await store.set("open_groups", groups);
      } catch (_) {}
    })();
  }, [groups, tabsRestored]);
  const groupsRef = useRef<Group[]>([]);
  useEffect(() => { groupsRef.current = groups; }, [groups]);
  // Which leaf inside an active group currently has focus (receives input).
  const [activeLeafByGroup, setActiveLeafByGroup] = useState<Record<string, string>>({});
  // Live drag state: which tab is being dragged, which leaf it's hovering over, which edge zone.
  const [dragOver, setDragOver] = useState<{ tabId: string; targetTabId: string | null; zone: DropZone | null } | null>(null);
  // Pointer position for rendering the floating drag ghost.
  const [dragPos, setDragPos] = useState<{ x: number; y: number } | null>(null);
  const workAreaRef = useRef<HTMLDivElement>(null);
  const groupCounterRef = useRef(1);

  // Stable DOM host per terminal tab — owned imperatively so React's reconciliation never
  // destroys them. Each host receives a portal-rendered <TerminalTab/> and is physically
  // reparented into the right slot (or the parking area) after every layout render.
  const terminalHostsRef = useRef<Map<string, HTMLDivElement>>(new Map());
  const parkingRef = useRef<HTMLDivElement>(null);
  const ensureHost = useCallback((tabId: string) => {
    let host = terminalHostsRef.current.get(tabId);
    if (!host) {
      host = document.createElement("div");
      host.className = "terminal-host";
      host.style.width = "100%";
      host.style.height = "100%";
      host.style.display = "flex";
      terminalHostsRef.current.set(tabId, host);
    }
    return host;
  }, []);

  // Global capture-phase listener: when the user clicks anywhere inside a pane belonging
  // to a group, mark that leaf as the focused one. We do this at the document level so
  // xterm's own internal event handlers can't shadow it.
  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      const tgt = e.target as HTMLElement | null;
      if (!tgt) return;
      const pane = tgt.closest("[data-group-leaf]") as HTMLElement | null;
      if (!pane) return;
      const leafId = pane.getAttribute("data-group-leaf");
      if (!leafId) return;
      const tab = tabsRef.current.find(t => t.id === leafId);
      if (!tab?.groupId) return;
      setActiveLeafByGroup(prev => (prev[tab.groupId!] === leafId ? prev : { ...prev, [tab.groupId!]: leafId }));
    };
    document.addEventListener("pointerdown", onDown, true);
    return () => document.removeEventListener("pointerdown", onDown, true);
  }, []);

  // Bump lastActiveAt on the currently focused tab whenever activeTabId or the focused
  // leaf inside a group changes. Powers the "recent" sort in the tab search dialog.
  useEffect(() => {
    const group = groupsRef.current.find(g => g.id === activeTabId);
    const id = group ? (activeLeafByGroup[activeTabId] || collectLeafIds(group.layout)[0]) : activeTabId;
    if (!id) return;
    const now = Date.now();
    setTabs(prev => {
      const found = prev.find(t => t.id === id);
      if (!found) return prev;
      return prev.map(t => t.id === id ? { ...t, lastActiveAt: now } : t);
    });
  }, [activeTabId, activeLeafByGroup]);

  // After each render: park every terminal host in its current slot (or the parking div).
  // Drop obsolete hosts for tabs that no longer exist.
  useLayoutEffect(() => {
    const liveIds = new Set(tabs.map(t => t.id));
    for (const [id, host] of Array.from(terminalHostsRef.current.entries())) {
      if (!liveIds.has(id)) {
        host.remove();
        terminalHostsRef.current.delete(id);
        continue;
      }
      const slot = document.querySelector(`[data-terminal-slot="${CSS.escape(id)}"]`) as HTMLElement | null;
      const target = slot || parkingRef.current;
      if (target && host.parentElement !== target) target.appendChild(host);
    }
  });

  // Derived: tab bar entries. A tab with groupId doesn't appear standalone — its group does.
  // Walking tabs in order yields a deterministic, order-preserving set of entries.
  // Memoized so the array reference is stable when tabs/groups don't change — the drag-reorder
  // hook in TabBar uses this as its `items` and would otherwise thrash its effect on every render.
  type Entry = { kind: "tab"; id: string; tab: Tab } | { kind: "group"; id: string; group: Group };
  const entries: Entry[] = useMemo(() => {
    const seen = new Set<string>();
    const out: Entry[] = [];
    for (const t of tabs) {
      if (t.groupId) {
        if (seen.has(t.groupId)) continue;
        const g = groups.find(gr => gr.id === t.groupId);
        if (!g) continue; // orphaned groupId — defensive skip
        seen.add(t.groupId);
        out.push({ kind: "group", id: g.id, group: g });
      } else {
        out.push({ kind: "tab", id: t.id, tab: t });
      }
    }
    return out;
  }, [tabs, groups]);

  // ── Remote Tabs mirror each Host's `terminals` list (ADR-0001) ──────
  // Buffered until the restore of open_tabs + cache has committed (amendment 21); then the
  // latest list per Host is applied. Lists already applied are skipped by identity.
  const appliedLiveRef = useRef<Record<string, TerminalInfo[]>>({});
  useEffect(() => {
    if (!tabsRestored) return;
    for (const [host, list] of Object.entries(hostsSnap.live)) {
      if (!list || appliedLiveRef.current[host] === list) continue;
      appliedLiveRef.current[host] = list;
      cache.putTerminals(host, list);
      metaSync.observe(host, list, tabsRef.current);
      const d = reconcile(tabsRef.current, host, list, pendingUuids(), (id, f) => metaSync.isDirty(id, f));
      for (const uuid of d.confirmed) if (pendingOpens.get(uuid)?.state === "sent") pendingOpens.delete(uuid);
      if (isEmptyDelta(d)) continue;
      for (const id of d.remove) {
        const t = tabsRef.current.find(x => x.id === id);
        if (t?.terminal) remoteTerminals.forget(t.terminal);
        metaSync.forget(id);
      }
      setTabs(prev => applyTabDelta(prev, d));
      if (d.remove.length) {
        setGroups(prev => applyGroupRemovals(prev, d.remove));
        // Amendment 22: a removed focused leaf hands focus to a surviving leaf.
        setActiveLeafByGroup(prev => applyFocusRemovals(prev, applyGroupRemovals(groupsRef.current, d.remove), d.remove));
        if (d.remove.includes(activeTabIdRef.current)) setActiveTabId("home");
      }
    }
  }, [tabsRestored, hostsSnap.live]);

  // Push local edits of remote Tabs (linked session id, title) to their Terminal's meta.
  // Only while the Host is usable; a failed call is retried once (amendment 17).
  const [metaTick, setMetaTick] = useState(0);
  useEffect(() => {
    if (!tabsRestored) return;
    const ready = tabs.filter(t => t.host && t.terminal && registry.isUsable(t.host));
    if (ready.length === 0) return;
    for (const u of metaSync.takeUpdates(ready)) {
      const args: Record<string, unknown> = { host: u.host, terminal: u.terminal };
      if (u.sessionId !== undefined) args.sessionId = u.sessionId;
      if (u.meta) args.meta = u.meta;
      invoke("host_term_update", args)
        .then(() => metaSync.settled(u, true))
        .catch(() => { metaSync.settled(u, false); if (metaSync.hasPending()) window.setTimeout(() => setMetaTick(n => n + 1), 2000); });
    }
  }, [tabs, tabsRestored, hostsSnap.status, metaTick]);

  // Dissolve a group when it has 0 or 1 leaves left; 1-leaf groups are pointless.
  useEffect(() => {
    const dissolved: string[] = [];
    const updatedTabs: Tab[] = [];
    let changed = false;
    for (const g of groups) {
      const leaves = collectLeafIds(g.layout);
      if (leaves.length <= 1) {
        dissolved.push(g.id);
        changed = true;
      }
    }
    if (!changed) return;
    for (const t of tabs) {
      if (t.groupId && dissolved.includes(t.groupId)) updatedTabs.push({ ...t, groupId: undefined });
      else updatedTabs.push(t);
    }
    setTabs(updatedTabs);
    setGroups(prev => prev.filter(g => !dissolved.includes(g.id)));
    // If the active entry was a dissolved group, switch to the remaining leaf (or home).
    if (dissolved.includes(activeTabIdRef.current)) {
      const survivors = tabs.filter(t => t.groupId && dissolved.includes(t.groupId));
      setActiveTabId(survivors[0]?.id || "home");
    }
  }, [groups, tabs]);

  // Drop a tab into the current work area. If `targetTabId` is a standalone tab, a new
  // group is created containing both. If it's inside a group, the dragged tab is inserted.
  const performDrop = useCallback((draggedTabId: string, targetTabId: string, zone: DropZone) => {
    if (draggedTabId === targetTabId) return;
    const dragged = tabsRef.current.find(t => t.id === draggedTabId);
    const target = tabsRef.current.find(t => t.id === targetTabId);
    if (!dragged || !target) return;
    if (dragged.groupId) return; // Already in a group — not allowed to re-add without removing first.

    if (target.groupId) {
      // Insert into the target's existing group.
      const group = groupsRef.current.find(g => g.id === target.groupId);
      if (!group) return;
      if (countLeaves(group.layout) >= MAX_GROUP_LEAVES) return;
      const newLayout = insertLeaf(group.layout, targetTabId, draggedTabId, zone);
      setGroups(prev => prev.map(g => g.id === group.id ? { ...g, layout: newLayout } : g));
      setTabs(prev => prev.map(t => t.id === draggedTabId ? { ...t, groupId: group.id } : t));
      setActiveTabId(group.id);
      setActiveLeafByGroup(prev => ({ ...prev, [group.id]: draggedTabId }));
    } else {
      // Both tabs are standalone → create a new group with both.
      const id = `group-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
      const name = `Group ${groupCounterRef.current++}`;
      const direction: "col" | "row" = zone === "left" || zone === "right" ? "col" : "row";
      const draggedLeaf: LayoutNode = { kind: "leaf", tabId: draggedTabId };
      const targetLeaf: LayoutNode = { kind: "leaf", tabId: targetTabId };
      const children: [LayoutNode, LayoutNode] = zone === "left" || zone === "top"
        ? [draggedLeaf, targetLeaf]
        : [targetLeaf, draggedLeaf];
      const layout: LayoutNode = { kind: "split", direction, children, ratio: 0.5 };
      setGroups(prev => [...prev, { id, name, layout }]);
      setTabs(prev => prev.map(t => (t.id === draggedTabId || t.id === targetTabId) ? { ...t, groupId: id } : t));
      setActiveTabId(id);
      setActiveLeafByGroup(prev => ({ ...prev, [id]: draggedTabId }));
    }
  }, []);

  // Close a pane in a group — removes the underlying tab entirely (matches the
  // expectation that × closes that terminal, not just ejects it).
  const closePaneInGroup = useCallback((tabId: string) => {
    const tab = tabsRef.current.find(t => t.id === tabId);
    if (!tab?.groupId) return;
    const group = groupsRef.current.find(g => g.id === tab.groupId);
    if (!group) return;
    markClosing([tab]);
    const nextLayout = removeLeaf(group.layout, tabId);
    if (nextLayout) {
      setGroups(prev => prev.map(g => g.id === group.id ? { ...g, layout: nextLayout } : g));
      // If the closed pane was the focused leaf, advance focus to another surviving leaf.
      setActiveLeafByGroup(prev => {
        if (prev[group.id] !== tabId) return prev;
        const survivors = collectLeafIds(nextLayout).filter(id => id !== tabId);
        const next = { ...prev };
        if (survivors.length) next[group.id] = survivors[0];
        else delete next[group.id];
        return next;
      });
    } else {
      setGroups(prev => prev.filter(g => g.id !== group.id));
      setActiveLeafByGroup(prev => { const n = { ...prev }; delete n[group.id]; return n; });
    }
    setTabs(prev => prev.filter(t => t.id !== tabId));
  }, []);

  // Adjust a split's ratio at the given path in the active group's layout tree.
  const updateGroupRatio = useCallback((groupId: string, path: number[], ratio: number) => {
    setGroups(prev => prev.map(g => {
      if (g.id !== groupId) return g;
      const next = setRatioAt(g.layout, path, ratio);
      return { ...g, layout: next };
    }));
  }, []);

  // ── Drag from tab bar → drop on a pane in the work area ───────
  // One pointerdown listener at the app level. It avoids touching useDragReorder so
  // intra-tab-bar reordering still works; once the pointer leaves the tab bar, we
  // enter split-drag mode and start painting drop zones over the pane under the cursor.
  useEffect(() => {
    let startTabId: string | null = null;
    let startX = 0, startY = 0;
    let dragging = false;

    // Find which leaf pane (and which zone of it) the pointer is over.
    const zoneAt = (x: number, y: number): { targetTabId: string | null; zone: DropZone | null } => {
      const area = workAreaRef.current;
      if (!area) return { targetTabId: null, zone: null };
      const areaRect = area.getBoundingClientRect();
      if (x < areaRect.left || x > areaRect.right || y < areaRect.top || y > areaRect.bottom) {
        return { targetTabId: null, zone: null };
      }
      // Pane-aware: if the active entry is a group, we want the specific pane the user
      // is over. Single-tab work areas carry a data-group-leaf on the wrapper.
      const hits = document.elementsFromPoint(x, y);
      let paneEl: HTMLElement | null = null;
      for (const el of hits) {
        const e = el as HTMLElement;
        if (e.dataset && e.dataset.groupLeaf) { paneEl = e; break; }
      }
      if (!paneEl) return { targetTabId: null, zone: null };
      const r = paneEl.getBoundingClientRect();
      const relX = (x - r.left) / r.width;
      const relY = (y - r.top) / r.height;
      // Split the pane into 4 triangles by its diagonals — every point inside the pane
      // falls into exactly one zone, so there's no dead middle.
      const dx = relX - 0.5;
      const dy = relY - 0.5;
      const zone: DropZone = Math.abs(dx) > Math.abs(dy)
        ? (dx < 0 ? "left" : "right")
        : (dy < 0 ? "top" : "bottom");
      return { targetTabId: paneEl.dataset.groupLeaf || null, zone };
    };

    const onDown = (e: PointerEvent) => {
      if (e.button !== 0) return;
      const tgt = e.target as HTMLElement | null;
      if (!tgt || tgt.closest(".tab-item-close")) return;
      const item = tgt.closest(".tab-item[data-drag-id]") as HTMLElement | null;
      if (!item) return;
      const id = item.getAttribute("data-drag-id");
      if (!id) return;
      startTabId = id;
      startX = e.clientX;
      startY = e.clientY;
      dragging = false;
    };
    const onMove = (e: PointerEvent) => {
      if (!startTabId) return;
      const dist = Math.hypot(e.clientX - startX, e.clientY - startY);
      if (!dragging && dist > 10) dragging = true;
      if (!dragging) return;
      const { targetTabId, zone } = zoneAt(e.clientX, e.clientY);
      // If the pointer is still inside the tab bar (no pane under it), let the intra-bar
      // reorder hook own the gesture — don't churn App state or show the split-drag ghost.
      // Return prev from the setters so React bails out (Object.is equality → no re-render).
      setDragOver(prev => {
        if (!prev && !targetTabId) return prev;
        return { tabId: startTabId!, targetTabId, zone };
      });
      setDragPos(prev => {
        if (!targetTabId && !prev) return prev;
        return { x: e.clientX, y: e.clientY };
      });
    };
    const onUp = (e: PointerEvent) => {
      if (startTabId && dragging) {
        const { targetTabId, zone } = zoneAt(e.clientX, e.clientY);
        if (targetTabId && zone) performDrop(startTabId, targetTabId, zone);
      }
      startTabId = null;
      dragging = false;
      setDragOver(null);
      setDragPos(null);
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, [performDrop]);

  // Sidebar only collapses via explicit user action (button in sidebar top, or chevron in TabBar).

  // Current project context: active terminal's project, or selected project on the project view.
  // Null on home (no context → hide + and dropdown).
  const contextProject: ProjectInfo | null = (() => {
    if (activeTabProjectPath && activeTabProjectKey) {
      const found = allProjects.find(p => sameKey(keyOf(p), activeTabProjectKey));
      if (found) return found;
      if (!activeTab?.projectName) return null;
      const fallback: ProjectInfo = { name: activeTab.projectName, path: activeTabProjectPath, encoded_name: "", session_count: 0, last_active: "" };
      return activeTab.host ? { ...fallback, host: activeTab.host } : fallback;
    }
    if (selectedProject) return selectedProject;
    return null;
  })();

  const handleNewChatInActive = useCallback((agent?: AgentId) => {
    if (contextProject) handleNewChat(contextProject, agent);
  }, [contextProject, handleNewChat]);

  // The + button in the tab bar: always open a raw shell using the user's default shell,
  // cwd = context project (or home if none).
  const handleNewShellInContext = useCallback(() => {
    // A Remote Host never gets the Desktop's default shell: use its OS's default instead.
    const shellId = contextProject?.host ? defaultShellForPlatform(registry.getStatus(contextProject.host)?.os ?? "linux") : defaultShell;
    const shell = getShellById(shellId);
    handleNewShell(contextProject, shellId, shell?.name || "Shell");
  }, [contextProject, defaultShell, handleNewShell]);

  // Group-aware tab selection: a tab inside a group requires activating its group AND
  // marking that pane as the focused leaf. Standalone tabs and groups themselves fall
  // through to a plain setActiveTabId. Used by both the tab bar and the search dialog.
  const handleSelectTab = useCallback((id: string) => {
    const tab = tabsRef.current.find(t => t.id === id);
    if (tab?.groupId) {
      setActiveTabId(tab.groupId);
      setActiveLeafByGroup(prev => ({ ...prev, [tab.groupId!]: id }));
    } else {
      setActiveTabId(id);
    }
  }, []);

  return (
    <div className={`app-layout ${sidebarCollapsed ? "sidebar-collapsed" : ""}`}>
      {/* Boot splash — full-window, screen-centered, drawn above everything (incl. the sidebar)
          so it isn't offset by the layout while the app loads. */}
      {initialLoading && (
        <div className="app-loading-overlay">
          <div className="spinner" />
          <span>Loading...</span>
        </div>
      )}
      <TabBar tabs={tabs} entries={entries} onRenameGroup={(id, name) => setGroups(prev => prev.map(g => g.id === id ? { ...g, name } : g))} closingTabIds={closingTabIds} activeTabId={activeTabId} selectedProject={selectedProject} hoveredProjectKey={hoveredProjectKey} linkedProjectKey={activeTabProjectKey} activeTabProject={contextProject} openSessionIds={new Set(tabs.map(sessionKeyOfTab).filter(Boolean) as string[])} projectIcons={projectIcons} pinnedProjects={userProjects} sidebarCollapsed={sidebarCollapsed} defaultShell={defaultShell} installedAgents={installedAgentsFor(contextProject?.host)} updateAvailable={updateInfo.updateAvailable} onExpandSidebar={() => setSidebarCollapsed(false)} onSelectTab={handleSelectTab} onCloseTab={handleCloseTab} onReorderTabs={handleReorderTabs} onNewChat={handleNewChat} onNewChatInActive={handleNewChatInActive} onNewShellInContext={handleNewShellInContext} onOpenSession={handleOpenSession} onNewShell={handleNewShell} onGoHome={handleGoHome} onOpenSettings={() => setActiveTabId("settings")} onToggleSidebar={() => setSidebarCollapsed(c => !c)} />
      <div className="app-body">
      <Sidebar projects={userProjects} projectIcons={projectIcons} selectedProject={selectedProject} activeCountByProject={activeCountByProject} sidebarLayout={sidebarLayout} onLayoutChange={persistSidebarLayout} onSelectProject={handleSelectProject} onGoHome={handleGoHome} onRemoveProject={handleRemoveProject} onEditProject={(k) => setEditingProjectKey(k)} onHoverProject={setHoveredProjectKey} onOpenSettings={() => setActiveTabId("settings")} onAddProject={() => setShowProjectPicker(true)} onCollapse={() => setSidebarCollapsed(true)} activeTabId={activeTabId} linkedProjectKey={activeTabProjectKey} showRateLimit={showRateLimitInSidebar} showRateLimitCodex={showRateLimitInSidebarCodex} updateAvailable={updateInfo.updateAvailable} />
      <div className="main-content">
        {/* Settings view — hidden unless activeTabId === 'settings' */}
        <div style={{ display: showSettings ? "flex" : "none", flex: 1, overflow: "hidden" }}>
          <SettingsView onSaveHosts={persistHosts} onRemoveHost={handleRemoveHost} theme={theme} onSetTheme={persistTheme} defaultAgent={defaultAgent} onSetDefaultAgent={persistDefaultAgent} gitLazyPolling={gitLazyPolling} onSetGitLazyPolling={persistGitLazyPolling} gitChangesTree={gitChangesTree} onSetGitChangesTree={persistGitChangesTree} fileExplorerOnStart={fileExplorerOnStart} onSetFileExplorerOnStart={persistFileExplorerOnStart} contextTreeEnabled={contextTreeEnabled} onSetContextTreeEnabled={persistContextTreeEnabled} terminalBgColor={terminalBgColor} onSetTerminalBgColor={persistTerminalBgColor} defaultTerminalFontSize={defaultTerminalFontSize} onSetDefaultTerminalFontSize={persistDefaultTerminalFontSize} alwaysOnTop={alwaysOnTop} onSetAlwaysOnTop={persistAlwaysOnTop} defaultShell={defaultShell} onSetDefaultShell={persistDefaultShell} fullscreenRendering={fullscreenRendering} onSetFullscreenRendering={persistFullscreenRendering} forceSyncOutput={forceSyncOutput} onSetForceSyncOutput={persistForceSyncOutput} webglRendering={webglRendering} onSetWebglRendering={persistWebglRendering} terminalFontWeight={terminalFontWeight} onSetTerminalFontWeight={persistTerminalFontWeight} eagerInitTabs={eagerInitTabs} onSetEagerInitTabs={persistEagerInitTabs} showRateLimitInSidebar={showRateLimitInSidebar} onSetShowRateLimitInSidebar={persistShowRateLimitInSidebar} showSessionRowMetrics={showSessionRowMetrics} onSetShowSessionRowMetrics={persistShowSessionRowMetrics} showSessionRowMetricsCodex={showSessionRowMetricsCodex} onSetShowSessionRowMetricsCodex={persistShowSessionRowMetricsCodex} showSessionRowMetricsOpencode={showSessionRowMetricsOpencode} onSetShowSessionRowMetricsOpencode={persistShowSessionRowMetricsOpencode} showRateLimitInSidebarCodex={showRateLimitInSidebarCodex} onSetShowRateLimitInSidebarCodex={persistShowRateLimitInSidebarCodex} showTerminalHeaderStats={showTerminalHeaderStats} onSetShowTerminalHeaderStats={persistShowTerminalHeaderStats} showProjectStatsChart={showProjectStatsChart} onSetShowProjectStatsChart={persistShowProjectStatsChart} updateInfo={updateInfo} />
        </div>
        {/* Home view — hidden when a terminal tab is active */}
        <div style={{ display: showHome ? "flex" : "none", flex: 1, overflow: "hidden" }}>
          <HomeView contextTreeEnabled={contextTreeEnabled} showSessionRowMetrics={showSessionRowMetrics} showSessionRowMetricsCodex={showSessionRowMetricsCodex} showSessionRowMetricsOpencode={showSessionRowMetricsOpencode} showProjectStatsChart={showProjectStatsChart} projects={userProjects} allProjects={allProjects} activeCountByProject={activeCountByProject} selectedProject={selectedProject} projectIcons={projectIcons} recentSessions={recentSessions} projectSessions={projectSessions} projectSessionsStale={projectSessionsStale} openSessionIds={new Set(tabs.map(sessionKeyOfTab).filter(Boolean) as string[])} sessionGroupName={(() => {
            const map: Record<string, string> = {};
            for (const t of tabs) {
              const sk = sessionKeyOfTab(t);
              if (sk && t.groupId) {
                const g = groups.find(gr => gr.id === t.groupId);
                if (g) map[sk] = g.name;
              }
            }
            return map;
          })()} loading={initialLoading} sessionsLoading={sessionsLoading} projectStatsView={projectStatsView} onChangeProjectStatsView={persistProjectStatsView} onOpenSession={handleOpenSession} onOpenSessionBackground={handleOpenSessionBackground} onSelectProject={handleSelectProject} onNewChat={handleNewChat} onAddProject={() => setShowProjectPicker(true)} onRemoveProject={handleRemoveProject} onEditProject={(k) => setEditingProjectKey(k)} onSaveFolders={handleSaveFolders} />
        </div>
        {/* Work area — shows the active entry (either a single tab or a group's split layout).
            Terminal DOM hosts (created imperatively below) are physically reparented into
            the relevant slots on each layout change; the TerminalTab React instance stays
            alive throughout, so its xterm + PTY are never re-spawned. */}
        <div ref={workAreaRef} className="work-area" style={{ display: showSettings || showHome ? "none" : "flex", flex: 1, overflow: "hidden", position: "relative" }}>
          {/* Standalone tabs render a bare slot (no React-level TerminalTab here). */}
          {tabs.filter(t => !t.groupId).map(tab => (
            <div key={tab.id} data-group-leaf={tab.id} className="work-pane" style={{ display: tab.id === activeTabId ? "flex" : "none" }}>
              <div className="terminal-slot" data-terminal-slot={tab.id} />
            </div>
          ))}
          {/* Group panes — the GroupView also renders slot divs for its leaves. */}
          {groups.map(g => {
            const isActive = g.id === activeTabId;
            const activeLeafId = activeLeafByGroup[g.id] || collectLeafIds(g.layout)[0] || null;
            return (
              <div key={g.id} className="work-pane" style={{ display: isActive ? "flex" : "none" }}>
                <GroupView
                  layout={g.layout}
                  activeLeafId={activeLeafId}
                  onFocusLeaf={(tabId) => setActiveLeafByGroup(prev => ({ ...prev, [g.id]: tabId }))}
                  onClosePane={closePaneInGroup}
                  onRatioChange={(path, ratio) => updateGroupRatio(g.id, path, ratio)}
                />
              </div>
            );
          })}
          {dragOver && dragOver.targetTabId && dragOver.zone && (
            <DropZoneOverlay targetTabId={dragOver.targetTabId} zone={dragOver.zone} />
          )}
        </div>

        {/* Floating drag ghost — a small pill with the dragged tab's label that follows the
            cursor while the user is dragging a tab into the work area. */}
        {dragOver && dragPos && (() => {
          const t = tabs.find(x => x.id === dragOver.tabId);
          if (!t) return null;
          const label = t.title || t.projectName || "Tab";
          return (
            <div className="tab-drag-ghost" style={{ top: dragPos.y + 12, left: dragPos.x + 14 }}>
              <span className="tab-drag-ghost-dot" />
              <span className="tab-drag-ghost-label">{label}</span>
              {t.projectName && <span className="tab-drag-ghost-sub">{t.projectName}</span>}
            </div>
          );
        })()}

        {/* Hidden parking area for terminal hosts that currently have no visible slot
            (inactive tabs, groups in the background). Keeps the React tree stable. */}
        <div ref={parkingRef} style={{ display: "none" }} aria-hidden />

        {/* Portal each TerminalTab into its stable DOM host. Because the host is a plain
            DOM node (not managed by React's child reconciliation for the work area),
            we can appendChild it into whichever slot corresponds to its current layout
            position without triggering an unmount — the xterm and PTY keep running. */}
        {tabs.map(tab => {
          const host = ensureHost(tab.id);
          // Look up the encoded claude-projects dir name for this tab's project so the
          // TerminalTab can pull cost/context stats. Empty string when the project hasn't
          // been seen by claude yet — TerminalTab handles that by hiding the stats strip.
          const tabKey = keyOfTab(tab);
          const encodedName = tabKey
            ? (allProjects.find(p => sameKey(keyOf(p), tabKey))?.encoded_name || "")
            : "";
          // The third arg is the portal's key — without it, this array reconciles by index,
          // so reordering tabs shuffles which host each portal targets and React remounts
          // the subtree (which kills the PTY in TerminalTab's cleanup). Keying by tab.id
          // makes a reorder a pure move — the TerminalTab instance, xterm, and PTY survive.
          return createPortal(
            <TerminalTab tab={tab} isActive={tab.id === activeTabId || (!!tab.groupId && tab.groupId === activeTabId && activeLeafByGroup[tab.groupId] === tab.id)} gitLazyPolling={gitLazyPolling} gitChangesTree={gitChangesTree} fileExplorerOnStart={fileExplorerOnStart} terminalBgColor={terminalBgColor} defaultFontSize={defaultTerminalFontSize} defaultShellId={defaultShell} fullscreenRendering={fullscreenRendering} forceSyncOutput={forceSyncOutput} webglRendering={webglRendering} terminalFontWeight={terminalFontWeight} eagerInit={eagerInitTabs} theme={theme} projectEncodedName={encodedName} showTerminalHeaderStats={showTerminalHeaderStats} onBranchSwitch={handleSwitchTabToBranch} />,
            host,
            tab.id,
          );
        })}
      </div>
      </div>
      {showProjectPicker && <ProjectPicker allProjects={allProjects} savedPaths={savedPaths} onToggle={handleToggleProject} onBrowse={() => { handleBrowseFolder(); setShowProjectPicker(false); }} onClose={() => setShowProjectPicker(false)} onRefresh={async () => { try { setAllProjects(await hostInvoke<ProjectInfo[]>(undefined, "list_claude_projects")); } catch (_) {} }} onAddRemotePath={handleAddRemotePath} />}
      {agentPickerProject && <AgentPickerDialog project={agentPickerProject} hostName={agentPickerProject.host ? registry.hostName(agentPickerProject.host) : undefined} agents={AGENT_IDS.filter(a => installedAgentsFor(agentPickerProject.host)[a])} onPick={(agent) => { const p = agentPickerProject; setAgentPickerProject(null); handleNewChat(p, agent); }} onClose={() => setAgentPickerProject(null)} onOpenSettings={() => { setAgentPickerProject(null); setActiveTabId("settings"); }} />}
      {editingProjectKey && (() => {
        const proj = allProjects.find(p => sameKey(keyOf(p), editingProjectKey)) || userProjects.find(p => sameKey(keyOf(p), editingProjectKey));
        if (!proj) { setEditingProjectKey(null); return null; }
        const settings = projectIcons[lookupKey(editingProjectKey)] || {};
        return <ProjectEditorDialog project={proj} settings={settings} onSave={(s) => handleSaveProjectSettings(editingProjectKey, s)} onClose={() => setEditingProjectKey(null)} />;
      })()}
      {updateDialogOpen && <UpdateDialog info={updateInfo} onDismiss={dismissUpdateDialog} />}
      <AppNotice notice={notice} onDismiss={dismissNotice} />
    </div>
  );
}
