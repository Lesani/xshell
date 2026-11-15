import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ArrowUpCircle, RefreshCcw } from "lucide-react";
import { ConfirmDialog } from "./ConfirmDialog";
import { fmt } from "../hosts/strings";
import { LOCAL_HOST, type LocalHostInfo, type LocalPersistentInfo } from "../hosts/localHost";
import { controls, errorCode, initialState, localDaemonLine, localDaemonVisible, otherBuildNote, reduce, startsUpgrade, statusErrorText, trackMounted, type Action, type State } from "../hosts/localDaemon";
import { C } from "../ring/copy";
import type { LocalStatus } from "../local/localInfo";
import { updateTarget, type UpdateTarget } from "../hosts/hostUpdate";
import { buildText } from "../buildId";
import { statusLabel, useHostsSnapshot } from "../hosts/useHosts";
import type { HostStatus, TerminalInfo } from "../hosts/types";

// Settings → Hosts → "This computer": the Local Host's Daemon (its build, upgrade) and whether
// closing xshell asks before leaving this machine's terminals running (D5). Self-contained.

// "Always keep terminals running in the background" (D5): `local_keep_sessions` is `ask` (the
// quit prompt asks) or `always`. Only where the Daemon is kept running (not on Windows).
export function KeepSessionsRow() {
  const [value, setValue] = useState<"ask" | "always" | null>(null);
  const [saving, setSaving] = useState(false);
  const [failed, setFailed] = useState(false);
  const mounted = useRef(false);
  useEffect(() => trackMounted(mounted), []);
  useEffect(() => {
    invoke<string>("local_keep_sessions_get").then(v => { if (mounted.current) setValue(v === "always" ? "always" : "ask"); }).catch(() => { if (mounted.current) setValue("ask"); });
  }, []);
  const change = async (on: boolean) => {
    const before = value;
    const next = on ? "always" : "ask";
    setValue(next);
    setSaving(true);
    setFailed(false);
    try {
      await invoke("local_keep_sessions_set", { value: next });
    } catch {
      // Not saved: the toggle shows what is stored.
      if (mounted.current) { setValue(before); setFailed(true); }
    } finally {
      if (mounted.current) setSaving(false);
    }
  };
  return (
    <>
      <div className="settings-row" data-keep-sessions>
        <div className="settings-row-text">
          <div className="settings-row-title">{C["quit.setting"]}</div>
          <div className="settings-row-desc">{C["quit.setting.body"]}</div>
        </div>
        <div className="settings-row-control">
          <label className={`settings-toggle ${value === null || saving ? "settings-toggle-disabled" : ""}`}>
            <input type="checkbox" aria-label={C["quit.setting"]} checked={value === "always"} disabled={value === null || saving} onChange={(e) => void change(e.target.checked)} />
            <span className="settings-toggle-slider" />
          </label>
        </div>
      </div>
      {failed && <div className="host-form-error" role="alert">{C["quit.setting.failed"]}</div>}
    </>
  );
}

const NO_PERSISTENT: LocalPersistentInfo = { supported: false, enabled: false, running: null, log: null };

export interface ViewProps {
  persistent: LocalPersistentInfo;
  // The keep-terminals setting applies (a kept-running Daemon, not Windows).
  keep?: boolean;
  status: HostStatus | undefined;
  live: TerminalInfo[] | null | undefined;
  state: State;
  dispatch: (a: Action) => void;
}

export function LocalDaemonView({ persistent, keep = false, status, live, state, dispatch }: ViewProps) {
  const c = controls(state, persistent, status, live);
  const line = localDaemonLine(status, persistent.running);
  const n = live?.length ?? 0;
  const pending = status?.status === "upgrade-pending" || status?.status === "build-differs" || status?.status === "incompatible";
  const other = otherBuildNote(status, persistent.running);
  const backendError = statusErrorText(status, state.error);
  const phase = state.phase;
  return (
    <div className="settings-section">
      <div className="settings-section-head">
        <div className="settings-section-title">{fmt("local.section.title")}</div>
      </div>
      <div className="settings-section-body">
        {keep && <KeepSessionsRow />}
        {state.error && <div className="host-row-error"><span className="host-row-error-text">{state.error}</span></div>}
        {backendError && (
          <div className="host-row-error">
            <span className="host-row-error-label">{fmt("hosts.row.lastError")}</span>
            <span className="host-row-error-text">{backendError}</span>
          </div>
        )}
        {(line || (pending && persistent.enabled)) && (
          <div className="host-row-info">
            {line && <span>{line}</span>}
            {pending && persistent.enabled && <span className={`host-chip host-chip-${status!.status}`}><span className="host-chip-dot" />{statusLabel(status)}</span>}
          </div>
        )}
        {other && <div className="host-row-note host-row-warn">{other}</div>}
        {(c.showUpgrade || c.showReplace) && (
          <div className="host-row-actions">
            {c.showUpgrade && (
              <button className="btn btn-ghost settings-action-btn" disabled={c.upgradeDisabled} onClick={() => dispatch({ type: "upgrade", terminals: n })}>
                <ArrowUpCircle size={11} /> {c.upgradeBusy ? fmt("hosts.row.upgrading") : fmt("hosts.row.upgrade")}
              </button>
            )}
            {c.showReplace && (
              <button className="btn btn-ghost settings-action-btn" disabled={c.upgradeDisabled} onClick={() => dispatch({ type: "replace", terminals: n, build: buildText(status?.desktopBuild) })}>
                <RefreshCcw size={11} /> {c.replaceBusy ? fmt("hosts.row.replacing") : fmt("hosts.row.replaceBuild")}
              </button>
            )}
          </div>
        )}
      </div>
      {(phase.kind === "confirm-switch" || phase.kind === "confirm-upgrade") && (
        <ConfirmDialog title={phase.confirm.title} body={phase.confirm.body} confirm={phase.confirm.confirm} onConfirm={() => dispatch({ type: "confirm" })} onCancel={() => dispatch({ type: "cancel" })} />
      )}
    </div>
  );
}

// What the settings page renders for the backend's `local_host_info`: nothing without a
// Local Daemon; its build and notes everywhere; persistence only where supported.
export function LocalDaemonSection({ info, ...rest }: Omit<ViewProps, "persistent" | "keep"> & { info: LocalHostInfo | null }) {
  if (!localDaemonVisible(info)) return null;
  // The always-on Daemon has no GUI-bound/Persistent mode any more: the raw (legacy) info, so
  // no mode wording is derived from it.
  return <LocalDaemonView persistent={info.persistent ?? NO_PERSISTENT} keep={(info as LocalStatus).service != null} {...rest} />;
}

export function LocalDaemonSettings() {
  const snap = useHostsSnapshot();
  const status = snap.status[LOCAL_HOST];
  const live = snap.live[LOCAL_HOST];
  const [info, setInfo] = useState<LocalHostInfo | null>(null);
  const [state, setState] = useState<State>(initialState);
  const stateRef = useRef(state);
  const confirmedTarget = useRef<UpdateTarget | null>(null);
  const mounted = useRef(false);
  useEffect(() => trackMounted(mounted), []);

  const refresh = useCallback(() => {
    invoke<LocalHostInfo>("local_host_info").then(i => { if (mounted.current) setInfo(i); }).catch(() => {});
  }, []);
  // The running mode changes with the connection.
  useEffect(refresh, [refresh, status?.status, status?.daemonVersion]);

  const dispatch = useCallback((a: Action) => {
    const prev = stateRef.current;
    if ((a.type === "upgrade" || a.type === "replace") && prev.phase.kind === "idle") {
      if (!status) return;
      confirmedTarget.current = updateTarget(status);
    }
    const next = reduce(prev, a);
    stateRef.current = next;
    setState(next);
    if (startsUpgrade(prev, next)) {
      invoke("host_upgrade", { host: LOCAL_HOST, target: confirmedTarget.current })
        .then(() => dispatch({ type: "upgraded" }))
        .catch(e => dispatch({ type: "failed", error: fmt("local.persistent.err.failed", { error: errorCode(e) }) }));
    }
  }, [status]);

  return <LocalDaemonSection info={info} status={status} live={live} state={state} dispatch={dispatch} />;
}
