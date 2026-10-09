import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Plus, RefreshCw, ArrowUpCircle, Pencil, Trash2, X, ChevronRight, PlugZap } from "lucide-react";
import { AGENT_IDS, AGENTS, AgentIcon } from "../agents";
import { ColorPicker } from "./ColorPicker";
import { HostBadge } from "./HostBadge";
import { ConfirmDialog } from "./ConfirmDialog";
import { LocalDaemonSettings } from "./LocalDaemonSettings";
import { fmt } from "../hosts/strings";
import { hintText, toHostConfig, validateHostForm, type HostFormErrors, type HostFormValues } from "../hosts/hostForm";
import { isUsableStatus } from "../hosts/registry";
import { phaseLabel, statusLabel, useHostsSnapshot } from "../hosts/useHosts";
import type { HostConfig, HostStatus, HostTestResult } from "../hosts/types";

// Settings → Hosts: add, edit, test, reconnect, upgrade and remove Remote Hosts.
// Saving persists the `hosts` key in settings.json and reconfigures the registry (App).

interface Props {
  onSave: (hosts: HostConfig[]) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
}

type TestState = { busy: boolean; text: string | null; ok: boolean };

function testText(r: HostTestResult, target: string): { text: string; ok: boolean } {
  if (!r.ok) {
    const hint = hintText(r.errorHint, target);
    return { text: fmt("hosts.test.failed", { error: r.error ?? "" }) + (hint ? ` ${hint}` : ""), ok: false };
  }
  if (r.installedVersion) return { text: fmt("hosts.test.ok", { os: r.os ?? "", arch: r.arch ?? "", version: r.installedVersion }), ok: true };
  return { text: fmt("hosts.test.okNotInstalled"), ok: true };
}

async function runTest(config: HostConfig): Promise<{ text: string; ok: boolean }> {
  try {
    return testText(await invoke<HostTestResult>("host_test", { config }), config.sshTarget);
  } catch (e) {
    return { text: fmt("hosts.test.failed", { error: typeof e === "string" ? e : (e as { message?: string })?.message ?? String(e) }), ok: false };
  }
}

// Re-render every second while a retry countdown is visible.
function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [active]);
  return now;
}

function StatusChip({ status, now }: { status: HostStatus | undefined; now: number }) {
  const phase = phaseLabel(status);
  const retry = status?.nextRetryAt && status.nextRetryAt > now ? fmt("hosts.status.retryIn", { s: Math.ceil((status.nextRetryAt - now) / 1000) }) : null;
  const kind = status?.status ?? "reconnecting";
  return (
    <span className={`host-chip host-chip-${kind}`}>
      <span className="host-chip-dot" />
      {statusLabel(status)}{phase ? ` · ${phase}` : ""}{retry ? ` · ${retry}` : ""}
    </span>
  );
}

function HostForm({ initial, onSave, onCancel }: { initial: HostConfig | null; onSave: (cfg: HostConfig) => void; onCancel: () => void }) {
  const [values, setValues] = useState<HostFormValues>({ name: initial?.name ?? "", sshTarget: initial?.sshTarget ?? "", color: initial?.color, daemonCommand: initial?.daemonCommand ?? "", launchPrefixes: { ...initial?.launchPrefixes } });
  const [errors, setErrors] = useState<HostFormErrors>({});
  const [advanced, setAdvanced] = useState(!!initial?.daemonCommand || Object.values(initial?.launchPrefixes ?? {}).some(Boolean));
  const [aliases, setAliases] = useState<string[]>([]);
  const [test, setTest] = useState<TestState>({ busy: false, text: null, ok: false });

  useEffect(() => { invoke<string[]>("list_ssh_hosts").then(setAliases).catch(() => {}); }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onCancel(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);

  const set = (patch: Partial<HostFormValues>) => { setValues(v => ({ ...v, ...patch })); setErrors({}); };
  const submit = () => {
    const e = validateHostForm(values);
    setErrors(e);
    if (Object.keys(e).length === 0) onSave(toHostConfig(values, initial?.id));
  };
  // Test the unsaved values (probe only: no install, no daemon start).
  const testNow = async () => {
    const e = validateHostForm(values);
    if (e.sshTarget || e.daemonCommand) { setErrors(e); return; }
    setTest({ busy: true, text: null, ok: false });
    const r = await runTest(toHostConfig({ ...values, name: values.name || values.sshTarget }, initial?.id));
    setTest({ busy: false, ...r });
  };

  return (
    <div className="settings-overlay" onClick={(e) => { if (e.target === e.currentTarget) onCancel(); }}>
      <div className="settings-panel host-form-panel">
        <div className="settings-header">
          <span>{initial ? fmt("hosts.form.titleEdit") : fmt("hosts.form.titleAdd")}</span>
          <button className="settings-close" onClick={onCancel} aria-label={fmt("hosts.form.cancel")}><X size={14} /></button>
        </div>
        <div className="settings-body">
          <div className="edit-field">
            <label className="edit-label">{fmt("hosts.form.name.label")}</label>
            <input autoFocus className="edit-input" value={values.name} placeholder={fmt("hosts.form.name.placeholder")} onChange={(e) => set({ name: e.target.value })} onKeyDown={(e) => { if (e.key === "Enter") submit(); }} />
            {errors.name && <div className="host-form-error">{errors.name}</div>}
          </div>
          <div className="edit-field">
            <label className="edit-label">{fmt("hosts.form.sshTarget.label")}</label>
            <input className="edit-input host-form-mono" list="xshell-ssh-aliases" value={values.sshTarget} placeholder={fmt("hosts.form.sshTarget.placeholder")} spellCheck={false} onChange={(e) => set({ sshTarget: e.target.value })} onKeyDown={(e) => { if (e.key === "Enter") submit(); }} />
            <datalist id="xshell-ssh-aliases">{aliases.map(a => <option key={a} value={a} />)}</datalist>
            {errors.sshTarget ? <div className="host-form-error">{errors.sshTarget}</div> : <div className="edit-hint">{fmt("hosts.form.sshTarget.help")}</div>}
          </div>
          <div className="edit-field">
            <ColorPicker label={fmt("hosts.form.color.label")} value={values.color} onChange={(color) => set({ color })} />
          </div>
          <div className="edit-field">
            <button className={`host-form-disclosure ${advanced ? "open" : ""}`} onClick={() => setAdvanced(a => !a)}><ChevronRight size={12} />{fmt("hosts.form.advanced")}</button>
            {advanced && (
              <div className="host-form-advanced">
                <label className="edit-label">{fmt("hosts.form.daemonCommand.label")}</label>
                <input className="edit-input host-form-mono" value={values.daemonCommand} placeholder={fmt("hosts.form.daemonCommand.placeholder")} spellCheck={false} onChange={(e) => set({ daemonCommand: e.target.value })} />
                {errors.daemonCommand ? <div className="host-form-error">{errors.daemonCommand}</div> : <div className="edit-hint">{fmt("hosts.form.daemonCommand.help")}</div>}
                <label className="edit-label host-form-subsection">{fmt("hosts.form.launchPrefix.label")}</label>
                {AGENT_IDS.map(agent => (
                  <div key={agent} className="host-form-prefix">
                    <span className="host-form-prefix-agent">{AGENTS[agent].label}</span>
                    <input className="edit-input host-form-mono" aria-label={`${fmt("hosts.form.launchPrefix.label")}: ${AGENTS[agent].label}`} value={values.launchPrefixes?.[agent] ?? ""} placeholder={fmt("hosts.form.launchPrefix.placeholder")} spellCheck={false} onChange={(e) => set({ launchPrefixes: { ...values.launchPrefixes, [agent]: e.target.value } })} />
                    {errors[`launchPrefix.${agent}`] && <div className="host-form-error">{errors[`launchPrefix.${agent}`]}</div>}
                  </div>
                ))}
                <div className="edit-hint">{fmt("hosts.form.launchPrefix.help")}</div>
              </div>
            )}
          </div>
          {test.text && <div className={`host-test-result ${test.ok ? "ok" : "failed"}`}>{test.text}</div>}
        </div>
        <div className="settings-footer">
          <button className="btn" disabled={test.busy} onClick={testNow}><PlugZap size={11} /> {test.busy ? fmt("hosts.row.testing") : fmt("hosts.row.test")}</button>
          <span style={{ flex: 1 }} />
          <button className="btn btn-ghost" onClick={onCancel}>{fmt("hosts.form.cancel")}</button>
          <button className="btn btn-primary" onClick={submit}>{fmt("hosts.form.save")}</button>
        </div>
      </div>
    </div>
  );
}

function HostRow({ host, onEdit, onRemove, now }: { host: HostConfig; onEdit: () => void; onRemove: () => void; now: number }) {
  const snap = useHostsSnapshot();
  const status = snap.status[host.id];
  const live = snap.live[host.id];
  const agents = snap.agents[host.id];
  const usable = isUsableStatus(status);
  const [test, setTest] = useState<TestState>({ busy: false, text: null, ok: false });
  const [details, setDetails] = useState(false);
  const [confirmUpgrade, setConfirmUpgrade] = useState(false);
  const [upgrading, setUpgrading] = useState(false);
  const canUpgrade = status?.status === "upgrade-pending" || (status?.status === "incompatible" && status.incompatibleReason === "daemon-older");
  const hint = hintText(status?.errorHint, host.sshTarget);
  const showError = !!status?.lastError && !usable;
  const firstLine = status?.lastError?.split("\n").find(l => l.trim()) ?? "";

  const testNow = async () => {
    setTest({ busy: true, text: null, ok: false });
    const r = await runTest(host);
    setTest({ busy: false, ...r });
  };
  const upgrade = async () => {
    setConfirmUpgrade(false);
    setUpgrading(true);
    try { await invoke("host_upgrade", { host: host.id }); } catch (_) {}
    setUpgrading(false);
  };

  return (
    <div className="host-row">
      <div className="host-row-head">
        <HostBadge host={host.id} size="md" />
        <div className="host-row-title">
          <span className="host-row-name">{host.name}</span>
          <span className="host-row-target">{host.sshTarget}</span>
        </div>
        <StatusChip status={status} now={now} />
      </div>

      {status?.status === "upgrade-pending" && <div className="host-row-note">{fmt("hosts.upgradePending.desc", { daemon: status.daemonVersion ?? "", desktop: status.desktopVersion })}</div>}
      {status?.status === "incompatible" && status.incompatibleReason && (
        <div className="host-row-note host-row-warn">{fmt(status.incompatibleReason === "daemon-older" ? "hosts.incompatible.older" : "hosts.incompatible.newer")}</div>
      )}
      {showError && (
        <div className="host-row-error">
          <span className="host-row-error-label">{fmt("hosts.row.lastError")}</span>
          <span className="host-row-error-text">{firstLine}</span>
          {hint && <div className="host-row-hint">{hint}</div>}
          {status!.lastError!.includes("\n") && <button className="host-row-link" onClick={() => setDetails(d => !d)}>{fmt("hosts.row.showDetails")}</button>}
          {details && <pre className="host-row-details">{status!.lastError}</pre>}
        </div>
      )}

      <div className="host-row-info">
        {status?.daemonVersion && <span>{fmt("hosts.row.daemonVersion", { v: status.daemonVersion })}</span>}
        {status?.desktopVersion && <span>{fmt("hosts.row.desktopVersion", { v: status.desktopVersion })}</span>}
        {live && <span>{fmt("hosts.row.terminals", { n: live.length })}</span>}
      </div>

      <div className="host-row-agents">
        <span className="host-row-agents-title">{fmt("hosts.row.agents.title")}</span>
        {usable && agents ? (
          AGENT_IDS.filter(a => agents[a]).map(a => (
            <span key={a} className="host-row-agent"><AgentIcon agent={a} size={12} /> {AGENTS[a].label}</span>
          ))
        ) : <span className="host-row-muted">{fmt("hosts.row.agents.unavailable")}</span>}
      </div>

      {test.text && <div className={`host-test-result ${test.ok ? "ok" : "failed"}`}>{test.text}</div>}

      <div className="host-row-actions">
        <button className="btn btn-ghost settings-action-btn" disabled={test.busy} onClick={testNow}><PlugZap size={11} /> {test.busy ? fmt("hosts.row.testing") : fmt("hosts.row.test")}</button>
        <button className="btn btn-ghost settings-action-btn" onClick={() => invoke("hosts_kick", { host: host.id }).catch(() => {})}><RefreshCw size={11} /> {fmt("hosts.row.reconnect")}</button>
        {canUpgrade && <button className="btn btn-ghost settings-action-btn" disabled={upgrading || status?.phase === "upgrading"} onClick={() => setConfirmUpgrade(true)}><ArrowUpCircle size={11} /> {upgrading || status?.phase === "upgrading" ? fmt("hosts.row.upgrading") : fmt("hosts.row.upgrade")}</button>}
        <span style={{ flex: 1 }} />
        <button className="btn btn-ghost settings-action-btn" onClick={onEdit}><Pencil size={11} /> {fmt("hosts.row.edit")}</button>
        <button className="btn btn-ghost settings-action-btn host-row-remove" onClick={onRemove}><Trash2 size={11} /> {fmt("hosts.row.remove")}</button>
      </div>

      {confirmUpgrade && (
        <ConfirmDialog title={fmt("hosts.upgrade.confirmTitle")} body={fmt("hosts.upgrade.confirmBody", { host: host.name, n: live?.length ?? 0 })} confirm={fmt("hosts.upgrade.confirm")} onConfirm={upgrade} onCancel={() => setConfirmUpgrade(false)} />
      )}
    </div>
  );
}

export function HostsSettings({ onSave, onRemove }: Props) {
  const snap = useHostsSnapshot();
  const hosts = snap.configs;
  const [editing, setEditing] = useState<HostConfig | "new" | null>(null);
  const [removing, setRemoving] = useState<HostConfig | null>(null);
  const now = useNow(hosts.some(h => !!snap.status[h.id]?.nextRetryAt));

  const save = async (cfg: HostConfig) => {
    const next = hosts.some(h => h.id === cfg.id) ? hosts.map(h => (h.id === cfg.id ? cfg : h)) : [...hosts, cfg];
    setEditing(null);
    await onSave(next);
  };

  return (
    <>
    <LocalDaemonSettings />
    <div className="settings-section">
      <div className="settings-section-head host-section-head">
        <div>
          <div className="settings-section-title">{fmt("hosts.section.title")}</div>
          <div className="settings-section-desc">{fmt("hosts.section.desc")}</div>
        </div>
        <button className="btn btn-primary settings-action-btn" onClick={() => setEditing("new")}><Plus size={11} /> {fmt("hosts.add")}</button>
      </div>
      <div className="settings-section-body">
        {hosts.length === 0 ? (
          <div className="host-empty">
            <div className="host-empty-title">{fmt("hosts.empty.title")}</div>
            <div className="host-empty-body">{fmt("hosts.empty.body")}</div>
          </div>
        ) : hosts.map(h => (
          <HostRow key={h.id} host={h} now={now} onEdit={() => setEditing(h)} onRemove={() => setRemoving(h)} />
        ))}
      </div>
      {editing && <HostForm initial={editing === "new" ? null : editing} onSave={save} onCancel={() => setEditing(null)} />}
      {removing && (
        <ConfirmDialog title={fmt("hosts.remove.confirmTitle")} body={fmt("hosts.remove.confirmBody", { host: removing.name })} confirm={fmt("hosts.remove.confirm")}
          onConfirm={() => { const id = removing.id; setRemoving(null); onRemove(id); }} onCancel={() => setRemoving(null)} />
      )}
    </div>
    </>
  );
}
