import { useEffect, useReducer, useRef, useState } from "react";
import { Smartphone } from "lucide-react";
import { fmt } from "../ring/strings";
import { pairingBlocked, pairingLine } from "../ring/mobileSettings";
import { errorText } from "../ring/members";
import { expiresLine, failureLine, offersNewCode, PHONE_IDLE, pairedLine, phoneReducer, type PhoneAction, type PhoneState, qrRects } from "../ring/pairing";
import type { PairingEvents } from "../ring/useRing";
import type { PhoneStart, Qr, RingStatus } from "../ring/types";

// Settings → Ring → Pair a phone (#9): a QR code (and its pairing text) the phone app scans.

export interface Pairing {
  events: PairingEvents;
  startPhone: (operation: string) => Promise<PhoneStart>;
  cancelPhone: (operation: string) => Promise<void>;
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
  const line = pairingLine(s, offerShown);
  if (!line) return null;
  return <div className="host-row-note host-row-warn" data-ring-connection-required>{line}</div>;
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
    if (acquiring.current || st.state === "starting" || st.state === "waiting" || pairingBlocked(s)) return;
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
  const blocked = pairingBlocked(s);
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
      {st.state === "failed" && <div className="host-form-error">{failureLine(st.code)}</div>}
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

