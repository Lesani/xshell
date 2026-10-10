import { Fragment, useEffect, useReducer, useState } from "react";
import { Monitor, Smartphone, Trash2 } from "lucide-react";
import { fmt } from "../ring/strings";
import { canClaim, canEnable, canRemove, canSave, connectionLine, errorText, hostLine, initialForm, lastSeenLine, localLine, moveLine, presenceChip, presenceKey, problemLine, removalNote, removalReducer, REMOVAL_IDLE, removeConfirm, removeHint, roleKey, startsOver, targetUrl, urlError, type RelayForm, type Removal } from "../ring/mobileSettings";
import { ConfirmDialog } from "./ConfirmDialog";
import { canSubmit, codeError, COMPUTER_IDLE, computerDesc, computerLine, computerReducer, expiresLine, failureLine, formatCode, normalizeCode, offersNewCode, PHONE_IDLE, pairedLine, phoneReducer, qrRects } from "../ring/pairing";
import { useRing, type PairingEvents } from "../ring/useRing";
import type { HostRingState, MemberView, PhoneStart, Qr, RingStatus } from "../ring/types";

// Settings → Mobile (#8, #21): enable Mobile access (creates the Ring), choose its Relay, see
// the Ring's devices and how they stand, and which Remote Hosts could not join. Pairing (#9):
// pair a phone by QR code, add a computer by the code `xshelld pair` shows. Removal (#22):
// remove a device from the list.

export interface Pairing {
  events: PairingEvents;
  startPhone: () => Promise<PhoneStart>;
  cancelPhone: () => Promise<void>;
  pairComputer: (code: string) => Promise<void>;
  cancelComputer: () => Promise<void>;
}

// Text with `code` spans, as the copy writes commands.
function WithCode({ text }: { text: string }) {
  return <>{text.split("`").map((part, i) => i % 2 ? <code key={i}>{part}</code> : <Fragment key={i}>{part}</Fragment>)}</>;
}

// Dark modules on white whatever the theme, with a four-module quiet zone.
function QrCode({ qr }: { qr: Qr }) {
  const n = qr.size + 8;
  return (
    <svg viewBox={`0 0 ${n} ${n}`} width={200} height={200} shapeRendering="crispEdges" role="img" aria-label={fmt("mobile.pair.phone")}>
      <rect width={n} height={n} fill="#fff" />
      {qrRects(qr).map(r => <rect key={`${r.x},${r.y}`} x={r.x + 4} y={r.y + 4} width={r.w} height={1} fill="#000" />)}
    </svg>
  );
}

function PhonePanel({ p }: { p: Pairing }) {
  const [st, dispatch] = useReducer(phoneReducer, PHONE_IDLE);
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => { if (p.events.phone) dispatch({ type: "event", event: p.events.phone }); }, [p.events.phone]);
  const waiting = st.state === "waiting";
  useEffect(() => {
    if (!waiting) return;
    const t = setInterval(() => {
      const n = Date.now();
      setNow(n);
      dispatch({ type: "tick", nowMs: n });
    }, 1000);
    return () => clearInterval(t);
  }, [waiting]);
  // Leaving Settings ends the offer: nobody sees the code any more.
  useEffect(() => () => { p.cancelPhone().catch(() => {}); }, []);

  const start = async () => {
    dispatch({ type: "start" });
    setNow(Date.now());
    try {
      dispatch({ type: "started", offer: await p.startPhone() });
    } catch (e) {
      dispatch({ type: "startFailed", error: errorText(e) });
    }
  };
  const busy = st.state === "starting" || waiting;
  return (
    <div className="edit-field">
      <label className="edit-label">{fmt("mobile.pair.phone")}</label>
      {st.state === "waiting" && (
        <>
          <div className="edit-hint">{fmt("mobile.pair.phone.desc")}</div>
          <QrCode qr={st.offer.qr} />
          <div className="host-row-note">{expiresLine(st.offer.expiresAt, now)}</div>
          <div className="host-row-note">{fmt("mobile.pair.waiting")}</div>
        </>
      )}
      {st.state === "paired" && <div className="host-row-note">{pairedLine(st.name)}</div>}
      {st.state === "expired" && <div className="host-row-note host-row-warn">{fmt("mobile.pair.expired")}</div>}
      {st.state === "failed" && <div className="host-form-error">{failureLine(st.code, st.error)}</div>}
      <div className="host-row-actions">
        {st.state === "waiting" ? (
          <button className="btn btn-ghost settings-action-btn" onClick={() => { navigator.clipboard?.writeText(st.offer.payload).catch(() => {}); }}>
            {fmt("mobile.pair.phone.copy")}
          </button>
        ) : (
          <button className="btn btn-primary settings-action-btn" disabled={busy} onClick={start}>
            <Smartphone size={11} /> {fmt(offersNewCode(st) ? "mobile.pair.phone.new" : "mobile.pair.phone")}
          </button>
        )}
      </div>
    </div>
  );
}

function ComputerPanel({ s, p }: { s: RingStatus; p: Pairing }) {
  const [st, dispatch] = useReducer(computerReducer, COMPUTER_IDLE);
  const [code, setCode] = useState("");
  useEffect(() => { if (p.events.computer) dispatch({ type: "event", event: p.events.computer }); }, [p.events.computer]);
  const connecting = st.state === "connecting";
  // Leaving Settings while it looks for the computer stops looking.
  useEffect(() => () => { p.cancelComputer().catch(() => {}); }, []);

  const submit = async () => {
    if (!canSubmit(code, st)) return;
    dispatch({ type: "submit" });
    try {
      await p.pairComputer(normalizeCode(code));
    } catch (e) {
      dispatch({ type: "submitFailed", error: errorText(e) });
    }
  };
  const invalid = codeError(code);
  const line = computerLine(st);
  return (
    <div className="edit-field">
      <label className="edit-label">{fmt("mobile.pair.computer")}</label>
      <div className="edit-hint"><WithCode text={computerDesc(s)} /></div>
      <div className="edit-field">
        <label className="edit-label">{fmt("mobile.pair.computer.label")}</label>
        <input className="edit-input host-form-mono" value={code} placeholder={fmt("mobile.pair.computer.placeholder")} spellCheck={false}
          autoCapitalize="characters" autoComplete="off" disabled={connecting}
          onChange={(e) => { setCode(formatCode(e.target.value)); dispatch({ type: "edit" }); }}
          onKeyDown={(e) => { if (e.key === "Enter") submit(); }} />
        {invalid && <div className="host-form-error">{invalid}</div>}
      </div>
      <div className="host-row-actions">
        <button className="btn btn-primary settings-action-btn" disabled={!canSubmit(code, st)} onClick={submit}>
          <Monitor size={11} /> {fmt("mobile.pair.computer")}
        </button>
      </div>
      {line && <div className={st.state === "failed" ? "host-form-error" : "host-row-note"}><WithCode text={line} /></div>}
    </div>
  );
}


function MemberRow({ m, s, removing, onRemove }: { m: MemberView; s: RingStatus; removing: boolean; onRemove: (m: MemberView) => void }) {
  const key = presenceKey(m, s);
  const seen = lastSeenLine(m, s);
  const hint = removeHint(m);
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
          {seen && <span className="host-row-muted">{seen}</span>}
        </div>
        <span className={`host-chip host-chip-${presenceChip(key)}`}><span className="host-chip-dot" />{fmt(key)}</span>
      </div>
      {hint && <div className="host-row-note">{hint}</div>}
      {canRemove(m, s) && (
        <div className="host-row-actions">
          <span style={{ flex: 1 }} />
          <button className="btn btn-ghost settings-action-btn host-row-remove" disabled={removing} onClick={() => onRemove(m)}>
            <Trash2 size={11} /> {removing ? fmt("mobile.member.removing") : fmt("mobile.member.remove")}
          </button>
        </div>
      )}
    </div>
  );
}

// The device list and a removal's note, which stays below the list after the removed row is
// gone.
export function MembersSection({ s, removal, onRemove, onClaimHost }: { s: RingStatus; removal: Removal; onRemove: (m: MemberView) => void; onClaimHost: (host: string) => Promise<void> }) {
  const note = removalNote(removal);
  const busy = removal.state === "removing";
  return (
    <div className="edit-field">
      <label className="edit-label">{fmt("mobile.members.title")}</label>
      {s.members.map(m => <MemberRow key={m.signKey} m={m} s={s} removing={busy && removal.signKey === m.signKey} onRemove={onRemove} />)}
      {note && <div className={note.error ? "host-form-error" : "host-row-note"}>{note.text}</div>}
      {s.hosts.map(h => <HostNote key={h.host} h={h} onClaim={onClaimHost} />)}
    </div>
  );
}

function HostNote({ h, onClaim }: { h: HostRingState; onClaim: (host: string) => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const claim = async () => {
    setBusy(true);
    setError(null);
    try {
      await onClaim(h.host);
    } catch (e) {
      setError(fmt("mobile.host.failed", { name: h.name, error: errorText(e) }));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className={`host-row-note ${h.state === "too-old" ? "" : "host-row-warn"}`}>
      {hostLine(h)}
      {canClaim(h) && (
        <div className="host-row-actions">
          <button className="btn settings-action-btn" disabled={busy} onClick={claim}>{fmt("mobile.host.claim")}</button>
        </div>
      )}
      {error && <div className="host-form-error">{error}</div>}
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

export function MobileSettingsView({ s, onEnable, onSaveRelay, onClaimHost, onRemoveMember, pairing }: { s: RingStatus; onEnable: (startOver: boolean) => Promise<void>; onSaveRelay: (url: string) => Promise<void>; onClaimHost: (host: string) => Promise<void>; onRemoveMember: (signKey: string) => Promise<RingStatus>; pairing?: Pairing }) {
  const [enabling, setEnabling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removal, dispatchRemoval] = useReducer(removalReducer, REMOVAL_IDLE);
  const remove = async () => {
    if (removal.state !== "confirming") return;
    const key = removal.member.signKey;
    dispatchRemoval({ type: "start" });
    try {
      dispatchRemoval({ type: "done", status: await onRemoveMember(key) });
    } catch (e) {
      dispatchRemoval({ type: "failed", error: e });
    }
  };
  const confirm = removal.state === "confirming" ? removeConfirm(removal.member) : null;
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
            <MembersSection s={s} removal={removal} onRemove={m => dispatchRemoval({ type: "ask", member: m })} onClaimHost={onClaimHost} />
            {confirm && <ConfirmDialog title={confirm.title} body={confirm.body} confirm={confirm.confirm} onConfirm={remove} onCancel={() => dispatchRemoval({ type: "cancel" })} />}
            {pairing && s.connection !== "other-window" && (
              <>
                <PhonePanel p={pairing} />
                <ComputerPanel s={s} p={pairing} />
              </>
            )}
          </>
        )}
        {local && <div className="host-row-note">{local}</div>}
      </div>
    </div>
  );
}

export function MobileSettings() {
  const { status, enable, setRelayUrl, claimHost, removeMember, pairing, startPhone, cancelPhone, pairComputer, cancelComputer } = useRing();
  if (!status) return null;
  const p: Pairing = { events: pairing, startPhone, cancelPhone, pairComputer, cancelComputer };
  return <MobileSettingsView s={status} onEnable={enable} onSaveRelay={setRelayUrl} onClaimHost={claimHost} onRemoveMember={removeMember} pairing={p} />;
}
