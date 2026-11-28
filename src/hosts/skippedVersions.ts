import { load } from "@tauri-apps/plugin-store";
import { formatBuild } from "../buildId";
import { updateAction, updateSource } from "./hostUpdate";
import type { HostId, HostStatus } from "./types";

// "Skip this version" in the update bar (Lesani/xshell#117): the bar stops offering one
// machine the update it offers now. Settings and "Update all" still offer it. Not the
// "skipped" of "Update all" (`skipReason`), which is a machine an update run left out.

export const SKIPPED_VERSIONS_SETTING = "update_skipped_versions";

/// What the offered update would install, as a key; `null` when nothing is offered. A
/// release is its version. This Desktop's build is its version and build, and whether it
/// updates or replaces: replacing the build of a version is another target than updating to
/// that version.
export function updateTargetKey(status: HostStatus | undefined): string | null {
  const action = updateAction(status);
  if (!status || !action) return null;
  if (updateSource(status) === "release") return `release:${status.updateVersion ?? status.desktopVersion}`;
  return `desktop:${action}:${status.desktopVersion}${status.desktopBuild ? `:${formatBuild(status.desktopBuild)}` : ""}`;
}

/// By machine row id: the target skipped there.
export type SkippedTargets = Record<HostId, string>;
type Snapshot = { loaded: boolean; skipped: SkippedTargets };
type SettingsStore = { get: <T>(key: string) => Promise<T | undefined>; set: (key: string, value: unknown) => Promise<void> };

function parse(raw: unknown): SkippedTargets {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return {};
  return Object.fromEntries(Object.entries(raw).filter(([, v]) => typeof v === "string" && v)) as SkippedTargets;
}

/// The bar hides this machine's offer: its skipped target is the one offered now. With
/// nothing offered now there is nothing to skip: an error of a later attempt (started from
/// Settings) stays visible. Until the stored targets are loaded every offer counts as
/// skipped, so a skipped row never flashes.
export function isVersionSkipped(snapshot: Snapshot, host: HostId, status: HostStatus | undefined): boolean {
  const offered = updateTargetKey(status);
  if (!snapshot.loaded) return offered !== null;
  return offered !== null && snapshot.skipped[host] === offered;
}

// Kept in settings.json, so a skip survives a restart. One target per machine: a new skip
// replaces the earlier one.
export class SkippedVersions {
  private state: Snapshot = { loaded: false, skipped: {} };
  private listeners = new Set<() => void>();
  private loading: Promise<void> | null = null;
  // Writes go one after another, each with the map as it is then: a later skip is never
  // overwritten by an earlier, slower write.
  private writing: Promise<void> = Promise.resolve();
  constructor(private open: () => Promise<SettingsStore> = () => load("settings.json", { defaults: {}, autoSave: true })) {}
  getSnapshot = () => this.state;
  // Subscribing (the mounted bar) starts the one load.
  subscribe = (listener: () => void) => { this.listeners.add(listener); void this.load(); return () => { this.listeners.delete(listener); }; };
  private set(state: Snapshot) { this.state = state; for (const listener of this.listeners) listener(); }
  load(): Promise<void> {
    return this.loading ??= (async () => {
      let stored: SkippedTargets = {};
      try { stored = parse(await (await this.open()).get<unknown>(SKIPPED_VERSIONS_SETTING)); } catch { /* unreadable settings: nothing is skipped */ }
      // A skip made while loading wins over the stored one.
      this.set({ loaded: true, skipped: { ...stored, ...this.state.skipped } });
    })();
  }
  /// Skips what `status` offers `host` now.
  skip(host: HostId, status: HostStatus | undefined) {
    const target = updateTargetKey(status);
    if (!target || this.state.skipped[host] === target) return;
    this.set({ ...this.state, skipped: { ...this.state.skipped, [host]: target } });
    this.writing = this.writing.then(() => this.load()).then(async () => { await (await this.open()).set(SKIPPED_VERSIONS_SETTING, this.state.skipped); }).catch(() => {});
  }
  /// A machine continues under another row id (#119: a saved SSH Host moved to the Ring
  /// list): its skipped target goes with it. A target `to` already has stays.
  move(from: HostId, to: HostId) {
    this.writing = this.writing.then(() => this.load()).then(async () => {
      const { [from]: target, ...rest } = this.state.skipped;
      if (target === undefined) return;
      this.set({ ...this.state, skipped: { ...rest, [to]: rest[to] ?? target } });
      await (await this.open()).set(SKIPPED_VERSIONS_SETTING, this.state.skipped);
    }).catch(() => {});
  }
}

export const skippedVersions = new SkippedVersions();
