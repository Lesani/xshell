import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ArrowUpCircle, RefreshCcw } from "lucide-react";
import { ConfirmDialog } from "./ConfirmDialog";
import { fmt } from "../hosts/strings";
import { LOCAL_HOST, type LocalHostInfo, type LocalPersistentInfo } from "../hosts/localHost";
import { confirmAgainCount, controls, effectivePersistent, errorCode, initialState, localDaemonLine, localDaemonVisible, otherBuildNote, reduce, startsSwitch, startsUpgrade, statusErrorText, switchErrorText, trackMounted, type Action, type State } from "../hosts/localDaemon";
import { buildText } from "../buildId";
import { statusLabel, useHostsSnapshot } from "../hosts/useHosts";
import type { HostStatus, TerminalInfo } from "../hosts/types";

// Settings → Hosts → "This computer": keep this computer's terminals running after quit (a
// Persistent Daemon, #25). Self-contained, so it can move to another settings page as is.

// Its own toggle: the shared one shows a disabled toggle as off, but this one keeps showing
// the stored value while a switch runs.
function PersistentToggle({ checked, disabled, onChange }: { checked: boolean; disabled: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className={`settings-toggle ${disabled ? "settings-toggle-disabled" : ""}`}>
      <input type="checkbox" aria-label={fmt("local.persistent.title")} checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span className="settings-toggle-slider" />
    </label>
  );
}

export interface ViewProps {
  persistent: LocalPersistentInfo;
  status: HostStatus | undefined;
  live: TerminalInfo[] | null | undefined;
  state: State;
  dispatch: (a: Action) => void;
}

export function LocalDaemonView({ persistent, status, live, state, dispatch }: ViewProps) {
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
        {persistent.supported && <div className="settings-section-desc">{fmt("local.section.desc")}</div>}
      </div>
      <div className="settings-section-body">
        {persistent.supported && <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-title">{fmt("local.persistent.title")}</div>
            <div className="settings-row-desc">{fmt("local.persistent.desc")}</div>
          </div>
          <div className="settings-row-control">
            <PersistentToggle checked={c.checked} disabled={c.disabled} onChange={(target) => dispatch({ type: "toggle", target, terminals: n })} />
          </div>
        </div>}
        {persistent.supported && c.hint && <div className="host-row-note">{c.hint}</div>}
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
export function LocalDaemonSection({ info, ...rest }: Omit<ViewProps, "persistent"> & { info: LocalHostInfo | null }) {
  if (!localDaemonVisible(info)) return null;
  return <LocalDaemonView persistent={effectivePersistent(info)} {...rest} />;
}

export function LocalDaemonSettings() {
  const snap = useHostsSnapshot();
  const status = snap.status[LOCAL_HOST];
  const live = snap.live[LOCAL_HOST];
  const [info, setInfo] = useState<LocalHostInfo | null>(null);
  const [state, setState] = useState<State>(initialState);
  const stateRef = useRef(state);
  const mounted = useRef(false);
  useEffect(() => trackMounted(mounted), []);

  const refresh = useCallback(() => {
    invoke<LocalHostInfo>("local_host_info").then(i => { if (mounted.current) setInfo(i); }).catch(() => {});
  }, []);
  // The running mode changes with the connection.
  useEffect(refresh, [refresh, status?.status, status?.daemonVersion]);

  const dispatch = useCallback((a: Action) => {
    const prev = stateRef.current;
    const next = reduce(prev, a);
    stateRef.current = next;
    setState(next);
    const start = startsSwitch(prev, next);
    if (start) {
      invoke<LocalHostInfo>("local_daemon_set_persistent", { enabled: start.target, confirmed: start.confirmed })
        .then(i => { if (mounted.current) setInfo(i); dispatch({ type: "switched" }); })
        .catch(e => {
          const code = errorCode(e);
          const again = confirmAgainCount(code);
          if (again !== null) dispatch({ type: "confirm-again", terminals: again });
          else dispatch({ type: "failed", error: switchErrorText(code, info?.persistent?.log) });
          refresh();
        });
    }
    if (startsUpgrade(prev, next)) {
      invoke("host_upgrade", { host: LOCAL_HOST })
        .then(() => dispatch({ type: "upgraded" }))
        .catch(e => dispatch({ type: "failed", error: fmt("local.persistent.err.failed", { error: errorCode(e) }) }));
    }
  }, [info, refresh]);

  return <LocalDaemonSection info={info} status={status} live={live} state={state} dispatch={dispatch} />;
}
