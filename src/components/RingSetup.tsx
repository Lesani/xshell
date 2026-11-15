import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import { load } from "@tauri-apps/plugin-store";
import { C, fmtc } from "../ring/copy";
import { OLD_SEEN_SETTING, SETUP_SKIPPED_SETTING, type StartView } from "../ring/firstLaunch";
import { canStart, countdownLine, liveOperation, setupReducer, setupView, SETUP_CHOICES, trackedOperation, wordGroups, type SetupAction, type SetupState } from "../ring/setup";
import { errorText } from "../ring/members";
import { useRing, type RingApi } from "../ring/useRing";
import { useNotAdded } from "../ring/notAdded";
import type { JoinOverview } from "../ring/types";

// The setup screen (D9): "This is my first machine" (Create Ring, nothing to fill in) or "Add
// this machine to my others" (the eight words; it continues by itself once another device adds
// it). Also the old-Ring notice and a removed machine's "Join again". It is a pseudo tab: leaving
// it unmounts it, which cancels the shown words (the glossary's cancel rule).

// How long Create / Show words wait for this machine's Host to register at first start.
const REGISTRATION_POLL_MS = 2000;
const REGISTRATION_POLLS = 15;

export type SetupVariant = StartView;

export interface RingSetupProps {
  variant: SetupVariant;
  // `join`: start the eight words at once (Join again from a removal notice).
  initial?: "join";
  // False while the screen is hidden (cancels the words, like leaving it).
  visible?: boolean;
  // Skip / Not now: the screen closes (Home).
  onClose: () => void;
  // Created or joined: the screen closes and shows the success line elsewhere.
  onDone: (text: string) => void;
  // For tests: the Ring hook.
  ring?: RingApi;
}

const sleep = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms));

export function RingSetup(props: RingSetupProps) {
  const own = useRing();
  return <RingSetupView {...props} ring={props.ring ?? own} />;
}

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

function RingSetupView({ variant, initial, visible = true, onClose, onDone, ring: r }: RingSetupProps & { ring: RingApi }) {
  const [st, setSt] = useReducer(setupReducer, SETUP_CHOICES);
  const stRef = useRef<SetupState>(st);
  const mounted = useRef(true);
  // Every start carries a generation; leaving bumps it, so a stale continuation calls nothing.
  const gen = useRef(0);
  // A Create or Show words is running (double submission is ignored).
  const inflight = useRef(false);
  // The operation whose `ring_join_start` was sent and not cancelled.
  const liveOp = useRef<string | null>(null);
  const rRef = useRef(r);
  rRef.current = r;
  const dispatch = useCallback((a: SetupAction) => {
    if (!mounted.current) return;
    stRef.current = setupReducer(stRef.current, a);
    setSt(a);
  }, []);

  // Leaving the code screen: the words die (U1, U4). Also ends a registration wait.
  const leave = useCallback(() => {
    gen.current++;
    inflight.current = false;
    const op = liveOp.current;
    liveOp.current = null;
    if (op) rRef.current.joinCancel(op).catch(() => {});
    const s = stRef.current;
    if (liveOperation(s) && op) dispatch({ type: "leave" });
    else dispatch({ type: "abort" });
  }, [dispatch]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      gen.current++;
      const op = liveOp.current;
      liveOp.current = null;
      mounted.current = false;
      if (op) rRef.current.joinCancel(op).catch(() => {});
    };
  }, []);
  useEffect(() => { if (!visible) leave(); }, [visible, leave]);
  useEffect(() => {
    const onHide = () => leave();
    window.addEventListener("pagehide", onHide);
    return () => window.removeEventListener("pagehide", onHide);
  }, [leave]);

  // This machine's Host must be registered before Create or Join can run (first start): re-read
  // the overview every 2 s, for up to 30 s.
  const waitRegistered = useCallback(async (my: number): Promise<"ok" | "stale" | "timeout"> => {
    const cached = rRef.current.overview;
    if (cached && cached.unavailable !== "not-registered") return "ok";
    for (let i = 0; i < REGISTRATION_POLLS; i++) {
      if (i > 0) {
        await sleep(REGISTRATION_POLL_MS);
        if (gen.current !== my) return "stale";
      }
      const o: JoinOverview | null = await rRef.current.refreshOverview();
      if (gen.current !== my) return "stale";
      if (o && o.unavailable !== "not-registered") return "ok";
    }
    return "timeout";
  }, []);

  const status = r.status;
  const { notAdded, clear: clearNotAdded } = useNotAdded(status);
  const follower = status?.connection === "other-window" || r.overview?.owner === false;
  const blocked = !status || status.busy || !r.relayReady || follower || status.problem === "unreadable";

  const create = useCallback(async () => {
    if (inflight.current || blocked || !canStart(stRef.current)) return;
    inflight.current = true;
    const my = ++gen.current;
    clearNotAdded();
    dispatch({ type: "create" });
    const ready = await waitRegistered(my);
    if (ready === "stale") return;
    if (ready === "timeout") {
      inflight.current = false;
      dispatch({ type: "createFailed", error: "not_registered: this machine's Host is not registered yet" });
      return;
    }
    try {
      await rRef.current.createComputer();
      if (gen.current === my) dispatch({ type: "created" });
    } catch (e) {
      if (gen.current === my) dispatch({ type: "createFailed", error: errorText(e) });
    } finally {
      if (gen.current === my) inflight.current = false;
    }
  }, [blocked, dispatch, waitRegistered, clearNotAdded]);

  const startJoin = useCallback(async () => {
    if (inflight.current || blocked) return;
    if (!canStart(stRef.current)) return;
    inflight.current = true;
    const my = ++gen.current;
    const op = crypto.randomUUID();
    clearNotAdded();
    dispatch({ type: "start", operation: op });
    const ready = await waitRegistered(my);
    if (ready === "stale") return;
    if (ready === "timeout") {
      inflight.current = false;
      dispatch({ type: "startFailed", operation: op, error: "not_registered: this machine's Host is not registered yet" });
      return;
    }
    liveOp.current = op;
    try {
      const started = await rRef.current.joinStart(op);
      if (gen.current !== my || !mounted.current) {
        // Left meanwhile: the words are never shown; make sure the backend drops them.
        rRef.current.joinCancel(op).catch(() => {});
        return;
      }
      dispatch({ type: "started", started });
    } catch (e) {
      if (liveOp.current === op) liveOp.current = null;
      dispatch({ type: "startFailed", operation: op, error: errorText(e) });
    } finally {
      if (gen.current === my) inflight.current = false;
    }
  }, [blocked, dispatch, waitRegistered, clearNotAdded]);

  // The backend's progress for the tracked operation.
  const tracked = trackedOperation(st);
  const event = tracked ? r.joinEvents[tracked] : undefined;
  useEffect(() => {
    if (!event) return;
    dispatch({ type: "event", event });
    // Past the code: the words are no longer live, nothing to cancel.
    if (event.state !== "waiting" && liveOp.current === event.operation) liveOp.current = null;
  }, [event, dispatch]);

  // The countdown.
  const counting = st.state === "code";
  const now = useClock(counting);
  useEffect(() => { if (counting) dispatch({ type: "tick", nowMs: now }); }, [counting, now, dispatch]);
  useEffect(() => { if (st.state === "ended" && liveOp.current) liveOp.current = null; }, [st.state]);

  // `ring:status` is authoritative (U3): finishing → none means the join did not complete; a
  // new removal returns to the choices (the removed variant).
  const membership = status?.membership;
  const prevMembership = useRef<string | undefined>(undefined);
  useEffect(() => {
    const prev = prevMembership.current;
    prevMembership.current = membership;
    if (prev === "finishing" && membership === "none") dispatch({ type: "force" });
  }, [membership, dispatch]);

  const removedAt = status ? (status.removed?.at ?? null) : undefined;
  const removedSeen = useRef<number | null | undefined>(undefined);
  useEffect(() => {
    if (removedAt === undefined) return;
    if (removedSeen.current === undefined) { removedSeen.current = removedAt; return; }
    if (removedAt === removedSeen.current) return;
    removedSeen.current = removedAt;
    // Removal decides for every local state, Approved. Finishing… included.
    if (removedAt !== null) { leave(); dispatch({ type: "force" }); }
  }, [removedAt, leave, dispatch]);

  // Join again from a removal notice: the words at once.
  const autoStarted = useRef(false);
  useEffect(() => {
    if (initial !== "join" || autoStarted.current || blocked || !r.overview) return;
    autoStarted.current = true;
    void startJoin();
  }, [initial, blocked, r.overview, startJoin]);

  const view = setupView(st, status);
  const doneText = view.kind === "done" ? fmtc(view.text) : null;
  const doneSent = useRef(false);
  useEffect(() => {
    if (doneText && !doneSent.current) { doneSent.current = true; onDone(doneText); }
  }, [doneText, onDone]);

  // The choices' variant follows the live status: a removal shows the removed screen.
  const shown: SetupVariant = membership === "removed" ? "removed" : variant === "old" && status?.oldRing ? "old" : variant === "removed" ? "first" : variant;

  const skip = async () => {
    try {
      const store = await load("settings.json", { defaults: {}, autoSave: true });
      if (shown === "old" && status?.oldRing) await store.set(OLD_SEEN_SETTING, status.oldRing.at);
      else if (shown === "first") await store.set(SETUP_SKIPPED_SETTING, true);
    } catch {
      // Not saved: the screen shows again at the next start.
    }
    onClose();
  };

  const titleId = "ring-setup-title";

  if (view.kind === "choices") {
    if (shown === "removed") {
      return (
        <div className="ring-setup" data-ring-setup="removed">
          <div className="ring-setup-inner" role="region" aria-labelledby={titleId}>
            <h1 id={titleId} className="ring-setup-title">{C["removed.title"]}</h1>
            <p className="ring-setup-body">{C["removed.body"]}</p>
            {follower && <p className="ring-setup-note">{C["ring.follower"]}</p>}
            {view.error && <p className="ring-setup-error" role="alert">{fmtc(view.error)}</p>}
            <div className="ring-setup-actions">
              <button className="btn btn-primary" data-ring-join-action disabled={blocked} onClick={() => void startJoin()}>{C["removed.action"]}</button>
              <button className="btn btn-ghost" data-ring-skip onClick={onClose}>{C["removed.dismiss"]}</button>
            </div>
          </div>
        </div>
      );
    }
    const old = shown === "old";
    return (
      <div className="ring-setup" data-ring-setup={shown}>
        <div className="ring-setup-inner" role="region" aria-labelledby={titleId}>
          <h1 id={titleId} className="ring-setup-title">{old ? C["old.title"] : C["first.title"]}</h1>
          <p className="ring-setup-body">{old ? C["old.body"] : C["first.body"]}</p>
          {old && <p className="ring-setup-body">{C["old.local"]}</p>}
          {follower && <p className="ring-setup-note">{C["ring.follower"]}</p>}
          {notAdded && <p className="ring-setup-error" role="alert" data-ring-not-added>{C["finish.notAdded"]}</p>}
          {view.error && <p className="ring-setup-error" role="alert">{fmtc(view.error)}</p>}
          <div className="ring-setup-cards">
            <section className="ring-setup-card" aria-labelledby="ring-setup-create">
              <h2 id="ring-setup-create" className="ring-setup-card-title">{old ? C["old.create.title"] : C["first.create.title"]}</h2>
              <p className="ring-setup-card-body">{old ? C["old.create.body"] : C["first.create.body"]}</p>
              <button className="btn btn-primary" data-ring-create disabled={blocked} onClick={() => void create()}>
                {view.retry ? C["create.retry"] : old ? C["old.create.action"] : C["first.create.action"]}
              </button>
            </section>
            <section className="ring-setup-card" aria-labelledby="ring-setup-join">
              <h2 id="ring-setup-join" className="ring-setup-card-title">{old ? C["old.join.title"] : C["first.join.title"]}</h2>
              <p className="ring-setup-card-body">{old ? C["old.join.body"] : C["first.join.body"]}</p>
              <button className="btn" data-ring-join-action disabled={blocked} onClick={() => void startJoin()}>{old ? C["old.join.action"] : C["first.join.action"]}</button>
            </section>
          </div>
          <div className="ring-setup-actions">
            <button className="btn btn-ghost ring-setup-skip" data-ring-skip onClick={() => void skip()}>{old ? C["old.skip"] : C["first.skip"]}</button>
          </div>
        </div>
      </div>
    );
  }

  const [top, bottom] = view.kind === "code" ? wordGroups(view.words) : [[], []];
  return (
    <div className="ring-setup" data-ring-setup={shown}>
      <div className="ring-setup-inner" role="region" aria-labelledby={titleId} aria-live="polite">
        {view.kind === "progress" && <h1 id={titleId} className="ring-setup-title">{fmtc(view.text)}</h1>}
        {view.kind === "finishing" && (
          <>
            <h1 id={titleId} className="ring-setup-title">{C["finish.title"]}</h1>
            <p className="ring-setup-body" data-ring-finishing>{view.offline ? C["finish.offline"] : C["finish.body"]}</p>
          </>
        )}
        {view.kind === "done" && <h1 id={titleId} className="ring-setup-title">{fmtc(view.text)}</h1>}
        {(view.kind === "joining" || view.kind === "code" || view.kind === "completing" || view.kind === "ended") && (
          <h1 id={titleId} className="ring-setup-title">{C["join.title"]}</h1>
        )}
        {view.kind === "joining" && <p className="ring-setup-body">{C["join.instruction"]}</p>}
        {view.kind === "code" && (
          <>
            <p className="ring-setup-body">{C["join.instruction"]}</p>
            <div className="ring-setup-words host-form-mono" data-ring-words>
              <div>{top.join(" ")}</div>
              <div>{bottom.join(" ")}</div>
            </div>
            <p className="ring-setup-countdown">{countdownLine(view.expiresAt, now)}</p>
            <p className="ring-setup-note">{C["join.lifetime"]}</p>
            <p className="ring-setup-note">{C["join.focus"]}</p>
            <p className="ring-setup-body">{C["join.waiting"]}</p>
          </>
        )}
        {view.kind === "completing" && <p className="ring-setup-body">{C["join.finishing"]}</p>}
        {view.kind === "ended" && <p className={view.action === "new" || view.text === "join.expired" ? "ring-setup-body" : "ring-setup-error"} role="alert">{fmtc(view.text)}</p>}
        <div className="ring-setup-actions">
          {view.kind === "ended" && view.action === "new" && (
            <button className="btn btn-primary" data-ring-new-code disabled={blocked} onClick={() => void startJoin()}>{C["join.new"]}</button>
          )}
          {view.kind === "ended" && view.action === "retry" && (
            <button className="btn btn-primary" data-ring-retry disabled={blocked} onClick={() => void startJoin()}>{C["join.retry"]}</button>
          )}
          {(view.kind === "joining" || view.kind === "code") && (
            <button className="btn btn-ghost" data-ring-cancel onClick={leave}>{C["join.cancel"]}</button>
          )}
          {view.kind === "ended" && (
            <button className="btn btn-ghost" data-ring-cancel onClick={() => dispatch({ type: "reset" })}>{C["join.cancel"]}</button>
          )}
        </div>
      </div>
    </div>
  );
}
