import { Fragment, useCallback, useEffect, useReducer, useRef, useState } from "react";
import { ArrowUpCircle, LogIn, Monitor, Plus, Smartphone, Trash2 } from "lucide-react";
import { fmt } from "../ring/strings";
import { canClaim, canEnable, canRemove, canSave, connectionLine, errorText, hostLine, initialForm, lastSeenLine, farewellLines, localLine, relayHost, quotaLine, pairingBlocked, pairingNote, presenceChip, presenceKey, problemLine, removalNote, removalReducer, REMOVAL_IDLE, memberRoleKey, removeConfirm, removeHint, startsOver, targetUrl, urlError, type RelayForm, type Removal } from "../ring/mobileSettings";
import { ConfirmDialog } from "./ConfirmDialog";
import { canSubmit, COMPUTER_IDLE, computerLine, computerReducer, type ComputerAction, type ComputerState, expiresLine, failureLine, normalizeWords, offersNewCode, PHONE_IDLE, pairedLine, phoneReducer, type PhoneAction, type PhoneState, qrRects, wordsError } from "../ring/pairing";
import { cliCardsFrom, cliLine, codeExpiresLine, consentBody, joinBusy, joinOperation, trackedOperation, JOIN_IDLE, joinReducer, offersNewCode as joinOffersNewCode, outcomeLine, showsJoinPanel, wordGroups, type JoinAction, type JoinUi } from "../ring/join";
import { useRing, type PairingEvents } from "../ring/useRing";
import type { ApprovedComputer, HostRingState, JoinEvent, JoinOverview, JoinPreview, JoinStarted, MemberView, PhoneStart, Qr, RingStatus } from "../ring/types";

// Settings → Hosts → Ring (#8, #21, #87): Create ring or Join ring (with this computer's
// local consent), choose the Ring's Relay, see its devices and how they stand, and which
// Remote Hosts could not join. Pairing (#9): pair a phone by QR code; add a computer by the
// eight words `xshelld pair` or Join ring shows. Removal (#22): remove a device from the list.

export interface Pairing {
  events: PairingEvents;
  startPhone: (operation: string) => Promise<PhoneStart>;
  cancelPhone: (operation: string) => Promise<void>;
  // Add computer: the normalized eight words.
  approveComputer: (words: string) => Promise<ApprovedComputer>;
}

// Computer adoption (#87): the commands and what they report.
export interface JoinApi {
  overview: JoinOverview | null;
  // The Relay chosen before Create/Join (null: hosted), bound into the operation.
  relay: string | null;
  // The saved choice has loaded and no Save is pending.
  relayReady: boolean;
  setRelay: (url: string | null) => Promise<void>;
  // The latest `ring:join` event per operation.
  events: Record<string, JoinEvent>;
  createComputer: () => Promise<void>;
  upgradeComputer: () => Promise<void>;
  start: (operation: string) => Promise<JoinStarted>;
  confirm: (operation: string) => Promise<void>;
  decline: (operation: string) => Promise<void>;
  cancel: (operation: string) => Promise<void>;
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

// Why pairing is disabled, or that it waits for the connection, under a panel's title.
// `offerShown`: a phone offer is up, and stays valid while the connection retries.
export function PairingNote({ s, offerShown = false }: { s: RingStatus; offerShown?: boolean }) {
  const key = pairingNote(s, offerShown);
  if (!key) return null;
  return <div className={`host-row-note ${key === "mobile.pair.connecting" ? "" : "host-row-warn"}`}>{fmt(key)}</div>;
}

// "Cancel pairing": ends the offer on the Desktop and returns the panel to "Pair a phone".
export async function cancelPhonePairing(cancelPhone: () => Promise<void>, dispatch: (a: PhoneAction) => void, operation: string): Promise<void> {
  dispatch({ type: "cancel", operation });
  try {
    await cancelPhone();
  } catch {
    // The offer ends by itself when it expires.
  }
}

export function PhonePanel({ s, p, initial = PHONE_IDLE }: { s: RingStatus; p: Pairing; initial?: PhoneState }) {
  const [st, dispatch] = useReducer(phoneReducer, initial);
  const operation = useRef<string | null>(null);
  const acquiring = useRef(false);
  const cancel = (owned: string) => {
    if (operation.current === owned) operation.current = null;
    return p.cancelPhone(owned);
  };
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
  useEffect(() => () => { if (operation.current) cancel(operation.current).catch(() => {}); }, []);

  const start = async () => {
    if (acquiring.current || st.state === "starting" || st.state === "waiting" || pairingBlocked(s) !== null) return;
    acquiring.current = true;
    const owned = crypto.randomUUID();
    operation.current = owned;
    dispatch({ type: "start", operation: owned });
    setNow(Date.now());
    try {
      dispatch({ type: "started", offer: await p.startPhone(owned) });
    } catch (e) {
      dispatch({ type: "startFailed", operation: owned, error: errorText(e) });
    } finally {
      acquiring.current = false;
    }
  };
  const busy = st.state === "starting" || waiting;
  const blocked = pairingBlocked(s) !== null;
  return (
    <div className="edit-field">
      <label className="edit-label">{fmt("mobile.pair.phone")}</label>
      <PairingNote s={s} offerShown={waiting} />
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
        ) : null}
        {st.state === "waiting" ? (
          <button className="btn btn-ghost settings-action-btn" onClick={() => { const owned = operation.current; if (owned) cancelPhonePairing(() => cancel(owned), dispatch, owned); }}>
            {fmt("mobile.pair.phone.cancel")}
          </button>
        ) : (
          <button className="btn btn-primary settings-action-btn" disabled={busy || blocked} onClick={() => { if (!blocked) start(); }}>
            <Smartphone size={11} /> {fmt(offersNewCode(st) ? "mobile.pair.phone.new" : "mobile.pair.phone")}
          </button>
        )}
      </div>
    </div>
  );
}

// "Add computer", from its button and from Enter alike: nothing while the words are not
// ready or pairing cannot succeed. Resolves true once the computer was added.
export async function submitComputer(words: string, st: ComputerState, s: RingStatus, approve: (words: string) => Promise<ApprovedComputer>, dispatch: (a: ComputerAction) => void, operation: string = crypto.randomUUID()): Promise<boolean> {
  if (!canSubmit(words, st) || pairingBlocked(s) !== null) return false;
  dispatch({ type: "submit", operation });
  try {
    const added = await approve(normalizeWords(words));
    dispatch({ type: "approved", operation, names: added.names });
    return true;
  } catch (e) {
    dispatch({ type: "submitFailed", operation, error: errorText(e) });
    return false;
  }
}

export function ComputerPanel({ s, p }: { s: RingStatus; p: Pairing }) {
  const [st, setSt] = useReducer(computerReducer, COMPUTER_IDLE);
  const [words, setWords] = useState("");
  // A reply after leaving Settings is not shown.
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const dispatch = (a: ComputerAction) => { if (mounted.current) setSt(a); };
  const connecting = st.state === "connecting";

  const submit = async () => {
    if (await submitComputer(words, st, s, p.approveComputer, dispatch) && mounted.current) setWords("");
  };
  const blocked = pairingBlocked(s) !== null;
  const invalid = wordsError(words);
  const line = computerLine(st);
  return (
    <div className="edit-field">
      <label className="edit-label">{fmt("mobile.pair.computer")}</label>
      <PairingNote s={s} />
      <div className="edit-hint"><WithCode text={fmt("mobile.pair.computer.desc")} /></div>
      <div className="edit-field">
        <label className="edit-label">{fmt("mobile.pair.computer.label")}</label>
        <input className="edit-input host-form-mono" value={words} placeholder={fmt("mobile.pair.computer.placeholder")} spellCheck={false}
          autoCapitalize="none" autoComplete="off" disabled={connecting} data-computer-words
          onChange={(e) => { setWords(e.target.value); dispatch({ type: "edit" }); }}
          onKeyDown={(e) => { if (e.key === "Enter") submit(); }} />
        {invalid && <div className="host-form-error">{invalid}</div>}
      </div>
      <div className="host-row-actions">
        <button className="btn btn-primary settings-action-btn" data-add-computer disabled={!canSubmit(words, st) || blocked} onClick={submit}>
          <Monitor size={11} /> {fmt("mobile.pair.computer")}
        </button>
      </div>
      {line && <div className={st.state === "failed" ? "host-form-error" : "host-row-note"}><WithCode text={line} /></div>}
    </div>
  );
}

// ── Computer adoption (#87) ───────────────────────────────────────────────

// The countdown's clock while `active`.
function useClock(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [active]);
  return now;
}

function Line({ line }: { line: { text: string; error: boolean } | null }) {
  if (!line) return null;
  return <div className={line.error ? "host-form-error" : "host-row-note"}>{line.text}</div>;
}

// Join this Ring? with Confirm/Decline. Read-only when `readOnly` (follower windows).
export function ConsentCard({ preview, busy, readOnly, nowMs, onConfirm, onDecline }: { preview: JoinPreview; busy: boolean; readOnly?: boolean; nowMs: number; onConfirm: () => void; onDecline: () => void }) {
  return (
    <div className="host-row" data-ring-consent>
      <div className="host-row-head"><div className="host-row-title"><span className="host-row-name">{fmt("ring.consentTitle")}</span></div></div>
      <div className="host-row-note">{consentBody(preview, "—")}</div>
      <div className="host-row-note">{codeExpiresLine(preview.expiresAt, nowMs)}</div>
      {!readOnly && (
        <div className="host-row-actions">
          <button className="btn btn-primary settings-action-btn" data-ring-confirm disabled={busy} onClick={onConfirm}>{fmt("ring.confirm")}</button>
          <button className="btn btn-ghost settings-action-btn" data-ring-decline disabled={busy} onClick={onDecline}>{fmt("ring.decline")}</button>
        </div>
      )}
    </div>
  );
}

// The Join ring screen: the words, then consent, then the outcome.
export function JoinPanel({ st, onCancel, onConfirm, onDecline, onNewCode }: { st: JoinUi; onCancel: () => void; onConfirm: () => void; onDecline: () => void; onNewCode: () => void }) {
  const counting = st.state === "code" || st.state === "consent" || st.state === "confirming";
  const now = useClock(counting);
  const [top, bottom] = st.state === "code" ? wordGroups(st.words) : [[], []];
  return (
    <div className="edit-field" data-ring-join>
      <label className="edit-label">{fmt("ring.joinTitle")}</label>
      {st.state === "code" && (
        <>
          <div className="edit-hint">{fmt("ring.joinInstruction")}</div>
          <div className="host-form-mono ring-code" data-ring-words style={{ fontSize: "1.5em", lineHeight: 1.6, userSelect: "all" }}>
            <div>{top.join(" ")}</div>
            <div>{bottom.join(" ")}</div>
          </div>
          <div className="host-row-note">{codeExpiresLine(st.expiresAt, now)}</div>
          <div className="host-row-note">{fmt("ring.waiting")}</div>
          <div className="edit-hint">{fmt("ring.codeLifetime")}</div>
        </>
      )}
      {(st.state === "consent" || st.state === "confirming") && (
        <ConsentCard preview={st.preview} busy={st.state === "confirming"} nowMs={now} onConfirm={onConfirm} onDecline={onDecline} />
      )}
      {st.state === "completing" && <div className="host-row-note">{fmt("ring.completing")}</div>}
      <Line line={outcomeLine(st)} />
      <div className="host-row-actions">
        {(st.state === "starting" || st.state === "code" || st.state === "consent") && (
          <button className="btn btn-ghost settings-action-btn" data-ring-cancel onClick={onCancel}>{fmt("ring.cancel")}</button>
        )}
        {joinOffersNewCode(st) && (
          <button className="btn btn-primary settings-action-btn" data-ring-new-code onClick={onNewCode}>{fmt("ring.generateNewCode")}</button>
        )}
      </div>
    </div>
  );
}

// "Pairing started from the command line on this computer": no words (the terminal shows them).
export function PairingWindowCard({ event, readOnly, onConfirm, onDecline }: { event: JoinEvent; readOnly: boolean; onConfirm: (operation: string) => Promise<void>; onDecline: (operation: string) => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const now = useClock(event.state === "awaitingConfirmation");
  const act = async (f: (operation: string) => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await f(event.operation);
    } catch (e) {
      setError(fmt("ring.joinFailed", { error: errorText(e) }));
      setBusy(false);
    }
  };
  return (
    <div className="edit-field" data-ring-cli={event.operation}>
      <label className="edit-label">{fmt("ring.cliStarted")}</label>
      {event.state === "awaitingConfirmation" && event.preview && (
        <ConsentCard preview={event.preview} busy={busy} readOnly={readOnly} nowMs={now} onConfirm={() => act(onConfirm)} onDecline={() => act(onDecline)} />
      )}
      <Line line={cliLine(event)} />
      {error && <div className="host-form-error">{error}</div>}
    </div>
  );
}

// Upgrade Ring: a Ring made by an older xshell, before this Desktop can add computers.
export function UpgradeCard({ s, onUpgrade }: { s: RingStatus; onUpgrade: () => Promise<void> }) {
  const [state, setState] = useState<{ kind: "idle" | "running" | "done" } | { kind: "failed"; error: string }>({ kind: "idle" });
  const unavailable = s.approval === "unavailable";
  const upgrade = async () => {
    setState({ kind: "running" });
    try {
      await onUpgrade();
      setState({ kind: "done" });
    } catch (e) {
      setState({ kind: "failed", error: errorText(e) });
    }
  };
  return (
    <div className="edit-field" data-ring-upgrade>
      <label className="edit-label">{fmt("ring.upgrade.title")}</label>
      <div className="edit-hint">{fmt("ring.upgrade.desc")}</div>
      {unavailable ? <div className="host-row-note host-row-warn">{fmt("ring.upgrade.unavailable")}</div> : (
        <div className="host-row-actions">
          <button className="btn btn-primary settings-action-btn" data-ring-upgrade-action disabled={state.kind === "running"} onClick={upgrade}>
            <ArrowUpCircle size={11} /> {state.kind === "running" ? fmt("ring.upgrade.running") : fmt("ring.upgrade.action")}
          </button>
        </div>
      )}
      {state.kind === "failed" && <div className="host-form-error">{state.error.startsWith("reconciliation_required") ? fmt("mobile.pair.err.reconcile") : fmt("ring.upgrade.failed", { error: state.error })}</div>}
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
            {fmt(memberRoleKey(m))}
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

// A v2 Ring's Relay is fixed: shown, not edited.
function FixedRelay({ s }: { s: RingStatus }) {
  return (
    <div className="edit-field" data-ring-relay-fixed>
      <label className="edit-label">{fmt("mobile.relay.title")}</label>
      <div className="host-row-note">{s.relayHosted ? fmt("mobile.relay.hosted") : (s.relayUrl ?? "")}</div>
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
          <div className="settings-row-desc">{fmt("mobile.relay.hosted.desc", { relay: relayHost(s.hostedRelayUrl) })}</div>
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

// Create ring / Join ring and the Join screen, while this Desktop is in no Ring. The Join
// state lives in the parent so the outcome stays visible once the Ring view takes over.
function UnpairedRing({ s, join, st, onCreate, onStart, unavailable }: { s: RingStatus; join: JoinApi; st: JoinUi; onCreate: () => void; onStart: () => void; unavailable: boolean }) {
  const busy = joinBusy(st);
  // Busy: another adoption, approval or registration runs; not ready: the Relay choice is
  // still loading or being saved.
  const blocked = unavailable || s.local === "in-process" || s.busy || !join.relayReady;
  return (
    <div className="edit-field" data-ring-unpaired>
      <div className="edit-hint">{fmt("ring.description")}</div>
      <div className="host-row-actions">
        <button className="btn btn-primary settings-action-btn" data-ring-create disabled={busy || blocked} onClick={onCreate}>
          <Plus size={11} /> {st.state === "creating" ? fmt("ring.creating") : fmt("ring.create")}
        </button>
        <button className="btn settings-action-btn" data-ring-join-action disabled={busy || blocked || !join} onClick={onStart}>
          <LogIn size={11} /> {fmt("ring.join")}
        </button>
      </div>
      {unavailable && <div className="host-row-note host-row-warn">{fmt("ring.unsupported")}</div>}
      <div className="edit-hint">{fmt("ring.localHint")}</div>
    </div>
  );
}

export function MobileSettingsView({ s, onEnable, onSaveRelay, onClaimHost, onRemoveMember, pairing, join, visible = true }: { s: RingStatus; onEnable: (startOver: boolean) => Promise<void>; onSaveRelay: (url: string) => Promise<void>; onClaimHost: (host: string) => Promise<void>; onRemoveMember: (signKey: string) => Promise<RingStatus>; pairing?: Pairing; join?: JoinApi; visible?: boolean }) {
  const [enabling, setEnabling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removal, dispatchRemoval] = useReducer(removalReducer, REMOVAL_IDLE);
  const [jst, setJst] = useReducer(joinReducer, JOIN_IDLE);
  const [upgraded, setUpgraded] = useState(false);
  // Replies after leaving Settings are not shown; the current operation is cancelled.
  const mounted = useRef(true);
  const op = useRef<string | null>(null);
  // The operation a cancel ends (the active one) and the one whose events still matter.
  op.current = joinOperation(jst);
  const tracked = trackedOperation(jst);
  const dispatchJoin = useCallback((a: JoinAction) => { if (mounted.current) setJst(a); }, []);
  const joinRef = useRef(join);
  joinRef.current = join;
  const cancelJoin = useCallback(() => {
    const owned = op.current;
    if (!owned) return;
    op.current = null;
    dispatchJoin({ type: "cancel", operation: owned });
    joinRef.current?.cancel(owned).catch(() => {});
  }, [dispatchJoin]);
  useEffect(() => {
    mounted.current = true;
    return () => {
      const owned = op.current;
      mounted.current = false;
      if (owned) joinRef.current?.cancel(owned).catch(() => {});
    };
  }, []);
  // The code dies with the screen: hidden Settings, another section, closing the page.
  useEffect(() => { if (!visible) cancelJoin(); }, [visible, cancelJoin]);
  useEffect(() => {
    const onHide = () => cancelJoin();
    window.addEventListener("pagehide", onHide);
    return () => window.removeEventListener("pagehide", onHide);
  }, [cancelJoin]);
  const current = tracked ? join?.events[tracked] : undefined;
  useEffect(() => { if (current) dispatchJoin({ type: "event", event: current }); }, [current, dispatchJoin]);
  const counting = jst.state === "code" || jst.state === "consent";
  useEffect(() => {
    if (!counting) return;
    const t = setInterval(() => dispatchJoin({ type: "tick", nowMs: Date.now() }), 1000);
    return () => clearInterval(t);
  }, [counting, dispatchJoin]);

  const create = async () => {
    if (!join || joinBusy(jst) || !join.relayReady) return;
    dispatchJoin({ type: "create" });
    try {
      await join.createComputer();
      dispatchJoin({ type: "created" });
    } catch (e) {
      dispatchJoin({ type: "createFailed", error: errorText(e) });
    }
  };
  const startJoin = async () => {
    if (!join || joinBusy(jst) || !join.relayReady) return;
    const owned = crypto.randomUUID();
    op.current = owned;
    dispatchJoin({ type: "start", operation: owned });
    try {
      const started = await join.start(owned);
      if (!mounted.current || op.current !== owned) {
        // Left meanwhile: the backend already cancelled it; make sure.
        join.cancel(owned).catch(() => {});
        return;
      }
      dispatchJoin({ type: "started", started });
    } catch (e) {
      dispatchJoin({ type: "startFailed", operation: owned, error: errorText(e) });
    }
  };
  const confirmJoin = async () => {
    const owned = op.current;
    if (!join || !owned || jst.state !== "consent") return;
    dispatchJoin({ type: "confirm", operation: owned });
    try {
      await join.confirm(owned);
    } catch (e) {
      dispatchJoin({ type: "confirmFailed", operation: owned, error: errorText(e) });
    }
  };
  const declineJoin = async () => {
    const owned = op.current;
    if (!join || !owned || jst.state !== "consent") return;
    try {
      await join.decline(owned);
      dispatchJoin({ type: "declined", operation: owned });
    } catch (e) {
      dispatchJoin({ type: "confirmFailed", operation: owned, error: errorText(e) });
    }
  };

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
  const follower = s.connection === "other-window" || join?.overview?.owner === false;
  const unavailable = join?.overview?.unavailable === "platform";
  const cliCards = join ? Object.values(cliCardsFrom(Object.values(join.events))).filter(e => e.state !== "cancelled") : [];
  const conn = connectionLine(s);
  const quota = quotaLine(s, Date.now());
  const farewells = farewellLines(s);
  const local = localLine(s);
  const problem = problemLine(s);
  const outcome = outcomeLine(jst);
  return (
    <div className="settings-section" data-ring-section>
      <div className="settings-section-head">
        <div className="settings-section-title">{fmt("ring.section.title")}</div>
        <div className="settings-section-desc">{fmt("mobile.section.desc")}</div>
      </div>
      <div className="settings-section-body">
        {problem && <div className="host-row-note host-row-warn">{problem}</div>}
        {s.problem ? (canEnable(s) &&
          // Only after unreadable settings were set aside: a new Ring starts over.
          <div className="host-row-actions">
            <button className="btn btn-primary settings-action-btn" disabled={enabling} onClick={enable}>
              <Smartphone size={11} /> {enabling ? fmt("mobile.enabling") : fmt("mobile.enable")}
            </button>
            {error && <div className="host-form-error">{error}</div>}
          </div>
        ) : !s.enabled ? (
          follower ? <div className="host-row-note">{fmt("ring.follower")}</div> : join && (
            showsJoinPanel(jst)
              ? <JoinPanel st={jst} onCancel={cancelJoin} onConfirm={confirmJoin} onDecline={declineJoin} onNewCode={startJoin} />
              : s.approval === "unresolved" && !joinBusy(jst)
                // Durable uncertainty from an earlier Create/Join: no new code or Ring.
                ? <div className="host-form-error" data-ring-unresolved>{fmt("ring.reconciliationRequired")}</div>
                : <>
                    <UnpairedRing s={s} join={join} st={jst} onCreate={create} onStart={startJoin} unavailable={unavailable} />
                    <Line line={outcome} />
                    <div data-ring-relay-unpaired>
                      <RelaySettings s={{ ...s, relayUrl: join.relay ?? s.hostedRelayUrl }} onSave={url => join.setRelay(url === s.hostedRelayUrl ? null : url)} />
                    </div>
                  </>
          )
        ) : (
          <>
            {(jst.state === "installed" || jst.state === "created") && <Line line={outcome} />}
            {s.format === 2 ? <FixedRelay s={s} /> : <RelaySettings s={s} onSave={onSaveRelay} />}
            {conn && <div className="host-row-note">{conn}</div>}
            {quota && <div className="host-row-note host-row-warn">{quota}</div>}
            {farewells.map((f, i) => <div key={i} className={`host-row-note ${f.warn ? "host-row-warn" : ""}`}>{f.text}</div>)}
            <MembersSection s={s} removal={removal} onRemove={m => dispatchRemoval({ type: "ask", member: m })} onClaimHost={onClaimHost} />
            {confirm && <ConfirmDialog title={confirm.title} body={confirm.body} confirm={confirm.confirm} onConfirm={remove} onCancel={() => dispatchRemoval({ type: "cancel" })} />}
            {follower ? <div className="host-row-note">{fmt("ring.follower")}</div> : (
              <>
                {join && (s.approval === "needs-upgrade" || s.approval === "needs-completion" || s.approval === "unavailable") && (
                  <UpgradeCard s={s} onUpgrade={async () => { await join.upgradeComputer(); if (mounted.current) setUpgraded(true); }} />
                )}
                {upgraded && s.approval === "ready" && <div className="host-row-note">{fmt("ring.upgrade.done")}</div>}
                {s.approval === "unresolved" && <div className="host-row-note host-row-warn">{fmt("mobile.pair.err.reconcile")}</div>}
                {pairing && (
                  <>
                    <PhonePanel s={s} p={pairing} />
                    {s.approval === "ready" && <ComputerPanel s={s} p={pairing} />}
                  </>
                )}
              </>
            )}
          </>
        )}
        {join && cliCards.map(e => <PairingWindowCard key={e.operation} event={e} readOnly={follower} onConfirm={join.confirm} onDecline={join.decline} />)}
        {local && <div className="host-row-note">{local}</div>}
      </div>
    </div>
  );
}

// The Ring section of Settings → Hosts. `visible`: Settings is shown on Hosts (Settings stays
// mounted when hidden, so leaving it must cancel a shown code).
export function MobileSettings({ visible = true }: { visible?: boolean }) {
  const r = useRing();
  if (!r.status) return null;
  const p: Pairing = { events: r.pairing, startPhone: r.startPhone, cancelPhone: r.cancelPhone, approveComputer: r.approveComputer };
  const join: JoinApi = {
    overview: r.overview, events: r.joinEvents, relay: r.chosenRelay, relayReady: r.relayReady, setRelay: r.setUnpairedRelay, createComputer: r.createComputer, upgradeComputer: r.upgradeComputer,
    start: r.joinStart, confirm: r.joinConfirm, decline: r.joinDecline, cancel: r.joinCancel,
  };
  return <MobileSettingsView s={r.status} onEnable={r.enable} onSaveRelay={r.setRelayUrl} onClaimHost={r.claimHost} onRemoveMember={r.removeMember} pairing={p} join={join} visible={visible} />;
}
