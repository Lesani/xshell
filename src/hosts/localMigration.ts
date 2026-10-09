import type { Group, LayoutNode, Tab } from "../types";
import { collectLeafIds } from "../layout";
import { remoteTabId } from "./reconcile";
import { fmt } from "./strings";
import { isHostError, type LaunchSpec, type TerminalInfo, type TerminalMeta } from "./types";

// Moving the saved in-process Local Tabs (`open_tabs`, from before local Daemon mode or from a
// run in fallback mode) into the local Daemon, once each, at startup (issue #4, ADR-0005).
//
// Each Tab gets a Terminal UUID derived from its id (`uuidV5(tab.id, MIGRATION_NS)`), so a
// re-run after a crash, or a second xshell instance, finds the Terminal instead of opening the
// session twice. There is no global "done" marker: every Daemon-mode start that finds such
// Tabs migrates them, which also covers Tabs a later fallback run creates.
//
// Pure pieces (plan, apply) plus an effectful runner and orchestrator with injected deps.

// A saved Tab that today's restore brings back in-process: an agent session of the Local Host.
export const isLegacyLocal = (t: Tab): boolean => !!(t.sessionId && t.projectPath && !t.host && !t.terminal);

const agentOf = (t: Tab): string => t.agent ?? "claude";
const sessionKey = (agent: string | null | undefined, sid: string) => `${agent || "claude"}\u0000${sid}`;

export interface MigrationOp {
  fromId: string;
  uuid: string;
  spec: LaunchSpec;
  meta: TerminalMeta;
}

export interface MigrationPlan {
  ops: MigrationOp[];
  // Every legacy Tab id → the Terminal it becomes: its own UUID (opened now, or already listed
  // after an earlier interrupted run), or the Terminal that already runs its session (M6/M8).
  target: Record<string, string>;
}

export interface PlanInput {
  saved: Tab[];
  live: TerminalInfo[];
  uuidOf: Record<string, string>;
  now: number;
  fullscreenRendering: boolean;
  forceSyncOutput: boolean;
}

// The spec matches a new Local Daemon agent Tab: the agent runs without the shell wrapper
// (sessionOps, Decision 9), and the Daemon resumes the session when its history exists.
export function migrationSpec(t: Tab, o: { fullscreenRendering: boolean; forceSyncOutput: boolean }): LaunchSpec {
  const spec: LaunchSpec = { agent: agentOf(t), sessionId: t.sessionId, cwd: t.projectPath!, shellMode: "claude", shellId: null, shellCommand: null, fullscreenRendering: o.fullscreenRendering, forceSyncOutput: o.forceSyncOutput };
  if (t.skipPermissions) spec.skipPermissions = true;
  return spec;
}

export function planLocalMigration(i: PlanInput): MigrationPlan {
  const listed = new Set(i.live.map(x => x.terminal));
  // (agent, session) → the Terminal reserved for it: listed ones first, then this batch's.
  const owner = new Map<string, string>();
  for (const x of i.live) if (x.spec.sessionId) { const k = sessionKey(x.spec.agent, x.spec.sessionId); if (!owner.has(k)) owner.set(k, x.terminal); }
  const ops: MigrationOp[] = [];
  const target: Record<string, string> = {};
  for (const t of i.saved) {
    if (!isLegacyLocal(t)) continue;
    const uuid = i.uuidOf[t.id];
    if (!uuid) continue;
    const k = sessionKey(t.agent, t.sessionId!);
    if (listed.has(uuid)) { target[t.id] = uuid; continue; }
    const other = owner.get(k);
    if (other) { target[t.id] = other; continue; }
    owner.set(k, uuid);
    target[t.id] = uuid;
    const meta: TerminalMeta = { title: t.title, createdAt: t.createdAt ?? t.lastActiveAt ?? i.now };
    if (t.projectName) meta.projectName = t.projectName;
    ops.push({ fromId: t.id, uuid, spec: migrationSpec(t, i), meta });
  }
  return { ops, target };
}

// "ok": the Terminal is listed. "failed": the Daemon refused it, so it does not exist and the
// Tab may run in-process. "unresolved": it may exist; the Tab is neither run nor dropped.
export type OpResult = "ok" | "failed" | "unresolved";

export interface ApplyInput {
  saved: Tab[];                        // `open_tabs`, saved order
  groups: Group[];                     // `open_groups`
  zoom: Record<string, number>;        // `terminal_zoom`
  plan: MigrationPlan;
  results: Record<string, OpResult>;   // by UUID
  live: TerminalInfo[];                // the confirmed "local" list after the run
}

export interface Applied {
  migrated: TerminalInfo[]; // the confirmed Terminals the saved Tabs became, oldest first
  inProcess: Tab[];       // refused: run in-process this run, retried next start
  heldBack: Tab[];        // unresolved: stay in `open_tabs`, not mounted, retried next start
  groups: Group[];
  zoom: Record<string, number>;
  migratedUuids: string[];
}

// Rewrites leaf ids through `rename`; a leaf mapped to null is removed (its split collapses).
function mapLeaves(n: LayoutNode, rename: (id: string) => string | null): LayoutNode | null {
  if (n.kind === "leaf") {
    const id = rename(n.tabId);
    return id === null ? null : id === n.tabId ? n : { kind: "leaf", tabId: id };
  }
  const a = mapLeaves(n.children[0], rename);
  const b = mapLeaves(n.children[1], rename);
  if (a === null) return b;
  if (b === null) return a;
  return a === n.children[0] && b === n.children[1] ? n : { ...n, children: [a, b] };
}

// Order is the Daemon's: by `createdAtMs`, ties by UUID (M9).
export const byDaemonOrder = (a: TerminalInfo, b: TerminalInfo) => a.createdAtMs - b.createdAtMs || (a.terminal < b.terminal ? -1 : a.terminal > b.terminal ? 1 : 0);

export function applyLocalMigration(i: ApplyInput): Applied {
  const listed = new Map(i.live.map(x => [x.terminal, x]));
  const opOwner = new Map(i.plan.ops.map(o => [o.uuid, o.fromId]));
  const newId: Record<string, string> = {};
  const inProcess: Tab[] = [];
  const heldBack: Tab[] = [];
  const firstOf = new Set<string>(); // UUIDs saved Tabs became
  for (const t of i.saved) {
    const uuid = i.plan.target[t.id];
    if (!uuid) continue;
    if (listed.has(uuid)) {
      newId[t.id] = remoteTabId(uuid);
      firstOf.add(uuid);
      continue;
    }
    // Only a refused Tab whose own open was refused runs in-process: a duplicate of another
    // Tab's session waits, so one session never gets two processes.
    if (opOwner.get(uuid) === t.id && i.results[uuid] === "failed") inProcess.push(t);
    else heldBack.push(t);
  }
  // Leaves that are not rewritten keep their ids; a rewritten leaf whose new id is already a
  // leaf somewhere (a duplicate of one session) is removed instead.
  const used = new Set<string>();
  for (const g of i.groups) for (const id of collectLeafIds(g.layout)) if (!(id in newId)) used.add(id);
  const groups: Group[] = [];
  for (const g of i.groups) {
    // Held-back leaves stay in the saved layout (pruned only where it is shown).
    const layout = mapLeaves(g.layout, id => {
      const n = newId[id];
      if (!n) return id;
      if (used.has(n)) return null;
      used.add(n);
      return n;
    });
    if (layout === g.layout) groups.push(g);
    else if (layout) groups.push({ ...g, layout });
  }
  const zoom = { ...i.zoom };
  for (const [from, to] of Object.entries(newId)) {
    if (typeof zoom[from] === "number" && zoom[to] === undefined) zoom[to] = zoom[from];
    delete zoom[from];
  }
  const migrated = [...firstOf.keys()].map(uuid => listed.get(uuid)!).sort(byDaemonOrder);
  return { migrated, inProcess, heldBack, groups, zoom, migratedUuids: migrated.map(x => x.terminal) };
}

// ── Runner ──────────────────────────────────────────────────────────

export const CONFIRM_MS = 15_000;
export const REFUSAL_CHECK_MS = 1_000;
export const READY_MS = 15_000;
export const LOCK_WAIT_MS = 2_000;

export interface RunDeps {
  // host_term_open on "local" (its temporary sinks detached on every path).
  open(op: MigrationOp): Promise<void>;
  // Whether `uuid` is (or within `ms` becomes) listed in the live "local" list.
  listed(uuid: string, ms: number): Promise<boolean>;
}

// A refusal proves no Terminal was created: the request never left (invalid, unknown Host) or
// the Daemon answered with an error other than "already exists". Everything else (timeout, a
// dropped link, "already exists", and "indeterminate": the Daemon started the Terminal but
// could not confirm it ended) is indeterminate (M2).
export function isRefusal(e: unknown): boolean {
  if (!isHostError(e)) return false;
  if (e.code === "invalid" || e.code === "unknown-host") return true;
  return e.code === "remote" && !/already exists/i.test(e.message);
}

// Opens one after another, in saved order. An open that cannot be confirmed stops the rest:
// they were never sent, so they cannot exist and count as refused.
export async function runLocalMigration(ops: MigrationOp[], deps: RunDeps): Promise<Record<string, OpResult>> {
  const results: Record<string, OpResult> = {};
  let stopped = false;
  for (const op of ops) {
    if (stopped) { results[op.uuid] = "failed"; continue; }
    let r: OpResult;
    try {
      await deps.open(op);
      // OK means the Daemon saved it (xshelld persists before answering); wait for the list.
      r = (await deps.listed(op.uuid, CONFIRM_MS)) ? "ok" : "unresolved";
    } catch (e) {
      // The join of open and attach can fail on the attach after a good open: check the list.
      if (isRefusal(e)) r = (await deps.listed(op.uuid, REFUSAL_CHECK_MS)) ? "ok" : "failed";
      else r = (await deps.listed(op.uuid, CONFIRM_MS)) ? "ok" : "unresolved";
    }
    results[op.uuid] = r;
    if (r === "unresolved") stopped = true;
  }
  return results;
}

// ── Orchestrator ────────────────────────────────────────────────────

// The saved state the migration rewrites.
export interface SavedLayout {
  openTabs: Tab[];
  openGroups: Group[];
  zoom: Record<string, number>;
}

// The migration journal (`local-migration.json`, written atomically by the Desktop's Rust
// side, outside settings.json). Written before the first open and kept until a start has
// applied it against a confirmed live list. `base`: the saved state as it was before any
// open; the result is derived from it again (deterministic UUIDs) by every start that finds
// it. `sent`: saved Tab ids whose Terminal may exist; such a Tab never runs in-process.
export interface Journal {
  version: 1;
  base?: SavedLayout;
  sent: string[];
}

export interface StoreState {
  openTabs: Tab[] | null | undefined;
  openGroups: Group[] | null | undefined;
  zoom: Record<string, number> | null | undefined;
}

export interface MigrationDeps extends RunDeps {
  lock(waitMs: number): Promise<boolean>;
  unlock(): Promise<void>;
  // This instance does not hold the lock: its store saves keep the disk's open_tabs,
  // open_groups and terminal_zoom.
  guard(): Promise<void>;
  read(): Promise<StoreState>;                  // fresh from disk (reload, ignoring defaults)
  write(s: SavedLayout): Promise<void>;         // the three keys, one save, fsynced
  readJournal(): Promise<Journal | null>;
  writeJournal(j: Journal): Promise<void>;
  clearJournal(): Promise<void>;
  ready(): Promise<TerminalInfo[] | null>;      // the live list once usable; null after READY_MS
  cached(): TerminalInfo[] | null;              // the last known "local" list (offline cache)
  live(): TerminalInfo[];
  uuid(tabId: string): Promise<string>;
  now(): number;
}

export interface MigrationOutcome {
  // Saved in-process Tabs to restore in-process (refused, or deferred and never sent).
  inProcess: Tab[];
  // Confirmed Terminals the saved Tabs became (merged with the cached list at restore).
  migrated: TerminalInfo[];
  // Kept in `open_tabs` and in the saved layouts, but not shown.
  heldBack: Tab[];
  // The saved `open_groups` to restore from; undefined: the startup snapshot's. Shown
  // through `renderedGroups`.
  groups?: Group[];
  // This instance holds the lock and writes `open_tabs`.
  ownsOpenTabs: boolean;
  // This instance writes `open_groups` and `terminal_zoom` (false without the lock).
  writesLayout: boolean;
  // Refused Tabs (for the notice).
  failed: number;
  // Call once restore has applied the outcome: retires (or narrows) the journal.
  settle(): Promise<void>;
}

const settled = async () => {};
export const NO_MIGRATION: MigrationOutcome = { inProcess: [], migrated: [], heldBack: [], ownsOpenTabs: false, writesLayout: true, failed: 0, settle: settled };

export interface MigrationSettings { fullscreenRendering: boolean; forceSyncOutput: boolean }

// One Tab per (agent, session); the others are returned as duplicates (M6).
export function dedupeBySession(tabs: Tab[]): { keep: Tab[]; dups: Tab[] } {
  const seen = new Set<string>();
  const keep: Tab[] = [], dups: Tab[] = [];
  for (const t of tabs) {
    const k = sessionKey(t.agent, t.sessionId ?? "");
    if (seen.has(k)) dups.push(t); else { seen.add(k); keep.push(t); }
  }
  return { keep, dups };
}

// The whole migration in local Daemon mode, given the startup snapshot of the store. A
// failing dependency gives an outcome that shows no saved in-process Tab and never rewrites
// the saved state: nothing runs twice and nothing saved is lost.
export async function migrateLocalTabs(snapshot: StoreState, settings: MigrationSettings, deps: MigrationDeps): Promise<MigrationOutcome> {
  try {
    // A journal that cannot be read is an error, never "no journal".
    const pending = await deps.readJournal();
    if (!(snapshot.openTabs ?? []).some(isLegacyLocal) && !pending) return NO_MIGRATION;
    return await migrate(settings, deps);
  } catch (_) {
    await deps.unlock().catch(() => {});
    await deps.guard().catch(() => {});
    return { ...NO_MIGRATION, writesLayout: false };
  }
}

const sessionOf = (t: Tab) => sessionKey(t.agent, t.sessionId ?? "");

async function migrate(settings: MigrationSettings, deps: MigrationDeps): Promise<MigrationOutcome> {
  // M5: another instance is migrating, or runs these Tabs in-process: leave them alone, and
  // never write the saved state back.
  if (!(await deps.lock(LOCK_WAIT_MS))) {
    await deps.guard();
    const fresh = await deps.read().catch(() => null);
    return { ...NO_MIGRATION, writesLayout: false, groups: fresh?.openGroups ?? undefined };
  }
  // Every journal change below happens under the lock, which is released only once the
  // outcome is settled (or kept while saved Tabs run in-process or wait).
  const journal = await deps.readJournal();
  let fresh: StoreState | null = null;
  try {
    fresh = await deps.read();
  } catch (e) {
    // settings.json is unreadable (a torn write): the journal's base rebuilds the keys.
    if (!journal?.base) throw e;
    await deps.write(journal.base);
  }
  const base: SavedLayout = journal?.base ?? { openTabs: fresh?.openTabs ?? [], openGroups: fresh?.openGroups ?? [], zoom: fresh?.zoom ?? {} };
  const sent = new Set(journal?.sent ?? []);
  const legacy = base.openTabs.filter(isLegacyLocal);
  if (legacy.length === 0) {
    await deps.clearJournal();
    await deps.unlock();
    return { ...NO_MIGRATION, groups: base.openGroups };
  }
  const ready = await deps.ready();
  if (!ready) {
    // Deferred: the Local Host is not ready. A Tab whose session the last known Daemon list
    // runs, or that the journal says may have a Terminal, waits; the others (one per
    // session) run in-process this run. The lock stays held so no other instance opens
    // these sessions meanwhile, and the journal stays as it is.
    const cached = deps.cached();
    const running = new Set((cached ?? []).filter(i => i.spec.sessionId).map(i => sessionKey(i.spec.agent, i.spec.sessionId!)));
    const mayExist = (t: Tab) => sent.has(t.id) || running.has(sessionOf(t));
    const { keep, dups } = dedupeBySession(legacy.filter(t => !mayExist(t)));
    const heldBack = legacy.filter(t => mayExist(t) || dups.includes(t));
    return { ...NO_MIGRATION, inProcess: keep, heldBack, groups: base.openGroups, ownsOpenTabs: true };
  }
  // Write-ahead: from here on these Tabs' Terminals may exist.
  await deps.writeJournal({ version: 1, base, sent: [...new Set([...sent, ...legacy.map(t => t.id)])] });
  const uuidOf: Record<string, string> = {};
  for (const t of legacy) uuidOf[t.id] = await deps.uuid(t.id);
  const plan = planLocalMigration({ saved: legacy, live: ready, uuidOf, now: deps.now(), ...settings });
  const results = await runLocalMigration(plan.ops, deps);
  const live = deps.live();
  const applied = applyLocalMigration({ saved: legacy, groups: base.openGroups, zoom: base.zoom, plan, results, live });
  const keep = new Set([...applied.inProcess, ...applied.heldBack].map(t => t.id));
  // A re-application of the journal: the same base and list give the same state. Durable
  // (fsynced) before the journal can be retired.
  await deps.write({ openTabs: legacy.filter(t => keep.has(t.id)), openGroups: applied.groups, zoom: applied.zoom });
  const owns = keep.size > 0;
  const heldIds = applied.heldBack.map(t => t.id);
  let settling: Promise<void> | null = null;
  return {
    inProcess: applied.inProcess,
    migrated: applied.migrated,
    heldBack: applied.heldBack,
    groups: applied.groups,
    ownsOpenTabs: owns,
    writesLayout: true,
    failed: applied.inProcess.length,
    // Held-back Tabs may still have a Terminal: only they stay marked. Once.
    settle: () => (settling ??= (async () => {
      if (heldIds.length) await deps.writeJournal({ version: 1, sent: heldIds });
      else await deps.clearJournal();
      if (!owns) await deps.unlock();
    })()),
  };
}

// M7: one migration per window, however often the startup runs; later callers share the
// first call's result.
let once: Promise<MigrationOutcome> | null = null;
export function migrateLocalTabsOnce(...args: Parameters<typeof migrateLocalTabs>): Promise<MigrationOutcome> {
  once ??= migrateLocalTabs(...args);
  return once;
}
export function _resetMigrationOnce() { once = null; }

// What `open_tabs` gets in local Daemon mode: nothing unless this instance holds the lock;
// then its in-process Tabs plus the held-back ones.
export function openTabsToPersist(persistable: Tab[], o: Pick<MigrationOutcome, "ownsOpenTabs" | "heldBack">): Tab[] | null {
  if (!o.ownsOpenTabs) return null;
  if (o.heldBack.length === 0) return persistable;
  const ids = new Set(persistable.map(t => t.id));
  return [...persistable, ...o.heldBack.filter(t => !ids.has(t.id))];
}

// The layouts as shown: held-back leaves pruned (a group left with one leaf dissolves).
export function renderedGroups(groups: Group[], heldBack: Tab[]): Group[] {
  if (heldBack.length === 0) return groups;
  const held = new Set(heldBack.map(t => t.id));
  const out: Group[] = [];
  for (const g of groups) {
    const layout = mapLeaves(g.layout, id => (held.has(id) ? null : id));
    if (layout === g.layout) out.push(g);
    else if (layout && layout.kind === "split") out.push({ ...g, layout });
  }
  return out;
}

// Same tree shape and leaf order, ignoring ratios and directions.
function sameShape(a: LayoutNode, b: LayoutNode): boolean {
  if (a.kind === "leaf" || b.kind === "leaf") return a.kind === b.kind && (a as { tabId: string }).tabId === (b as { tabId: string }).tabId;
  return sameShape(a.children[0], b.children[0]) && sameShape(a.children[1], b.children[1]);
}

// `saved` with the shown edits of `shown` (= `saved` pruned, same shape) merged in: ratios
// and directions of the splits that are shown.
function mergeShown(saved: LayoutNode, shown: LayoutNode, held: Set<string>): LayoutNode {
  if (saved.kind === "leaf") return saved;
  const a = mapLeaves(saved.children[0], id => (held.has(id) ? null : id));
  const b = mapLeaves(saved.children[1], id => (held.has(id) ? null : id));
  if (!a) return { ...saved, children: [saved.children[0], mergeShown(saved.children[1], shown, held)] };
  if (!b) return { ...saved, children: [mergeShown(saved.children[0], shown, held), saved.children[1]] };
  if (shown.kind !== "split") return saved;
  return { ...saved, direction: shown.direction, ratio: shown.ratio, children: [mergeShown(saved.children[0], shown.children[0], held), mergeShown(saved.children[1], shown.children[1], held)] };
}

const heldLeaf = (tabId: string): LayoutNode => ({ kind: "leaf", tabId });

// What `open_groups` gets (F): the shown groups with the held-back leaves of their saved
// groups merged back in, so a hidden leaf is never dropped. A shown group with the saved
// shape keeps the saved tree with the shown ratios; one the user restructured gets its
// held-back leaves appended. A saved group that is no longer shown is kept while its shown
// leaves are still open and ungrouped, or as a group of its held-back leaves if there are
// two or more. Null: this instance does not write layouts.
export function groupsToPersist(shown: Group[], tabs: Tab[], saved: Group[], o: Pick<MigrationOutcome, "heldBack" | "writesLayout">): Group[] | null {
  if (!o.writesLayout) return null;
  if (o.heldBack.length === 0) return shown;
  const held = new Set(o.heldBack.map(t => t.id));
  const keepSaved = saved.filter(g => collectLeafIds(g.layout).some(id => held.has(id)));
  if (keepSaved.length === 0) return shown;
  const open = new Set(tabs.map(t => t.id));
  const inShown = new Set(shown.flatMap(g => collectLeafIds(g.layout)));
  const out: Group[] = [];
  for (const g of shown) {
    const h = keepSaved.find(x => x.id === g.id);
    if (!h) { out.push(g); continue; }
    const pruned = mapLeaves(h.layout, id => (held.has(id) ? null : id));
    if (pruned && sameShape(pruned, g.layout)) { out.push({ ...g, layout: mergeShown(h.layout, g.layout, held) }); continue; }
    let layout = g.layout;
    for (const id of collectLeafIds(h.layout)) if (held.has(id)) layout = { kind: "split", direction: "row", ratio: 0.5, children: [layout, heldLeaf(id)] };
    out.push({ ...g, layout });
  }
  for (const h of keepSaved) {
    if (shown.some(g => g.id === h.id)) continue;
    const ids = collectLeafIds(h.layout);
    const visible = ids.filter(id => !held.has(id));
    if (visible.every(id => open.has(id) && !inShown.has(id))) { out.push(h); continue; }
    const hidden = mapLeaves(h.layout, id => (held.has(id) ? id : null));
    if (hidden && hidden.kind === "split") out.push({ ...h, layout: hidden });
  }
  return out;
}

// The notice after a start where some saved Tabs could not move, or null.
export function migrationNotice(failed: number): string | null {
  if (failed <= 0) return null;
  return failed === 1 ? fmt("notice.localMigration.partialOne") : fmt("notice.localMigration.partial", { count: failed });
}
