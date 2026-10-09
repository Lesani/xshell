import { useEffect, useState } from "react";
import { Smartphone } from "lucide-react";
import { fmt } from "../ring/strings";
import { canEnable, canSave, connectionLine, errorText, initialForm, localLine, moveLine, presenceChip, presenceKey, problemLine, roleKey, startsOver, targetUrl, urlError, type RelayForm } from "../ring/mobileSettings";
import { useRing } from "../ring/useRing";
import type { MemberView, RingStatus } from "../ring/types";

// Settings → Mobile (#8): enable Mobile access (creates the Ring), choose its Relay, and see
// the Ring's devices and how they stand.

function MemberRow({ m, s }: { m: MemberView; s: RingStatus }) {
  const key = presenceKey(m, s);
  return (
    <div className="host-row">
      <div className="host-row-head">
        <div className="host-row-title">
          <span className="host-row-name">{m.name}</span>
          <span className="host-row-target">
            {fmt(roleKey(m.role))}
            {m.thisApp ? ` · ${fmt("mobile.member.thisApp")}` : ""}
            {m.thisComputer ? ` · ${fmt("mobile.member.thisComputer")}` : ""}
          </span>
        </div>
        <span className={`host-chip host-chip-${presenceChip(key)}`}><span className="host-chip-dot" />{fmt(key)}</span>
      </div>
    </div>
  );
}

function RelaySettings({ s, onSave }: { s: RingStatus; onSave: (url: string) => Promise<void> }) {
  const [form, setForm] = useState<RelayForm>(() => initialForm(s));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Follow the Ring when its Relay changes (here or from another device).
  useEffect(() => { setForm(initialForm(s)); }, [s.relayUrl, s.hostedRelayUrl]);

  const save = async () => {
    if (!canSave(form, s, busy)) return;
    setBusy(true);
    setError(null);
    try {
      await onSave(targetUrl(form, s));
    } catch (e) {
      setError(fmt("mobile.relay.err.saveFailed", { error: errorText(e) }));
    } finally {
      setBusy(false);
    }
  };
  const invalid = urlError(form);
  return (
    <div className="edit-field">
      <label className="edit-label">{fmt("mobile.relay.title")}</label>
      <label className="settings-row">
        <input type="radio" name="mobile-relay" checked={form.choice === "hosted"} onChange={() => setForm({ ...form, choice: "hosted" })} />
        <div className="settings-row-text">
          <div className="settings-row-title">{fmt("mobile.relay.hosted")}</div>
          <div className="settings-row-desc">{fmt("mobile.relay.hosted.desc")}</div>
        </div>
      </label>
      <label className="settings-row">
        <input type="radio" name="mobile-relay" checked={form.choice === "custom"} onChange={() => setForm({ ...form, choice: "custom" })} />
        <div className="settings-row-text">
          <div className="settings-row-title">{fmt("mobile.relay.custom")}</div>
        </div>
      </label>
      {form.choice === "custom" && (
        <div className="edit-field">
          <label className="edit-label">{fmt("mobile.relay.url.label")}</label>
          <input className="edit-input host-form-mono" value={form.url} placeholder={fmt("mobile.relay.url.placeholder")} spellCheck={false}
            onChange={(e) => setForm({ ...form, url: e.target.value })} onKeyDown={(e) => { if (e.key === "Enter") save(); }} />
          {invalid ? <div className="host-form-error">{invalid}</div> : <div className="edit-hint">{fmt("mobile.relay.url.help")}</div>}
        </div>
      )}
      {form.choice === "hosted" && <div className="edit-hint">{fmt("mobile.relay.url.help")}</div>}
      <div className="host-row-actions">
        <button className="btn btn-primary settings-action-btn" disabled={!canSave(form, s, busy)} onClick={save}>{fmt("mobile.relay.save")}</button>
      </div>
      {error && <div className="host-form-error">{error}</div>}
    </div>
  );
}

export function MobileSettingsView({ s, onEnable, onSaveRelay }: { s: RingStatus; onEnable: (startOver: boolean) => Promise<void>; onSaveRelay: (url: string) => Promise<void> }) {
  const [enabling, setEnabling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const enable = async () => {
    setEnabling(true);
    setError(null);
    try {
      await onEnable(startsOver(s));
    } catch (e) {
      setError(fmt("mobile.enable.failed", { error: errorText(e) }));
    } finally {
      setEnabling(false);
    }
  };
  const conn = connectionLine(s);
  const move = moveLine(s);
  const local = localLine(s);
  const problem = problemLine(s);
  return (
    <div className="settings-section">
      <div className="settings-section-head">
        <div className="settings-section-title">{fmt("mobile.section.title")}</div>
        <div className="settings-section-desc">{fmt("mobile.section.desc")}</div>
      </div>
      <div className="settings-section-body">
        {problem && <div className="host-row-note host-row-warn">{problem}</div>}
        {s.problem || !s.enabled ? (canEnable(s) &&
          <div className="host-row-actions">
            <button className="btn btn-primary settings-action-btn" disabled={enabling} onClick={enable}>
              <Smartphone size={11} /> {enabling ? fmt("mobile.enabling") : fmt("mobile.enable")}
            </button>
            {error && <div className="host-form-error">{error}</div>}
          </div>
        ) : (
          <>
            <RelaySettings s={s} onSave={onSaveRelay} />
            {conn && <div className="host-row-note">{conn}</div>}
            {move && <div className={`host-row-note ${s.move?.state === "failed" ? "host-row-warn" : ""}`}>{move}</div>}
            <div className="edit-field">
              <label className="edit-label">{fmt("mobile.members.title")}</label>
              {s.members.map(m => <MemberRow key={m.signKey} m={m} s={s} />)}
            </div>
          </>
        )}
        {local && <div className="host-row-note">{local}</div>}
      </div>
    </div>
  );
}

export function MobileSettings() {
  const { status, enable, setRelayUrl } = useRing();
  if (!status) return null;
  return <MobileSettingsView s={status} onEnable={enable} onSaveRelay={setRelayUrl} />;
}
