import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { load } from "@tauri-apps/plugin-store";
import type { ApprovedComputer, JoinEvent, JoinOverview, JoinStarted, PairingEvent, PairingFlow, PhoneStart, RingStatus } from "./types";

// The Relay chosen on the unpaired screen (settings.json), bound into Create and Join. Absent:
// the hosted Relay.
export const RING_RELAY_SETTING = "ring_relay_url";

// The latest `ring:pairing` event of each flow (a new object per event).
export type PairingEvents = Record<PairingFlow, PairingEvent | null>;

// The Ring as Settings → Hosts → Ring shows it: `ring_status` once, then every `ring:status`;
// phone pairing (#9): its commands and every `ring:pairing`; Computer adoption (#87): its
// commands, `ring_join_overview` once and every `ring:join`.
export function useRing() {
  const [status, setStatus] = useState<RingStatus | null>(null);
  const [pairing, setPairing] = useState<PairingEvents>({ phone: null, computer: null });
  // The latest `ring:join` event per operation (GUI and CLI-started alike).
  const [joinEvents, setJoinEvents] = useState<Record<string, JoinEvent>>({});
  const [overview, setOverview] = useState<JoinOverview | null>(null);
  const [chosenRelay, setChosenRelay] = useState<string | null>(null);
  // Create/Join wait for the saved choice and for a Save in progress.
  const [relayLoaded, setRelayLoaded] = useState(false);
  const [relaySaving, setRelaySaving] = useState(false);
  const relayRef = useRef<string | null>(null);
  const mounted = useRef(false);
  const operations = useRef<Partial<Record<PairingFlow, string>>>({});

  useEffect(() => {
    mounted.current = true;
    const unlisten: (() => void)[] = [];
    let cancelled = false;
    const keep = (u: () => void) => { if (cancelled) u(); else unlisten.push(u); };
    // Ownership can move between windows: every status change and focus request re-reads it.
    const refreshOverview = () => {
      invoke<JoinOverview>("ring_join_overview").then(o => {
        if (!mounted.current || !o) return;
        setOverview(o);
        // Events seen meanwhile are newer than the overview.
        setJoinEvents(m => ({ ...Object.fromEntries(o.joins.map(j => [j.operation, j])), ...m }));
      }).catch(() => {});
    };
    listen<RingStatus>("ring:status", e => { if (mounted.current) { setStatus(e.payload); refreshOverview(); } })
      .then(keep)
      .catch(() => {});
    listen("ring:focus", () => { if (mounted.current) refreshOverview(); })
      .then(keep)
      .catch(() => {});
    // An unpaired takeover need not change any status: re-read when the window gains focus.
    const onFocus = () => { if (mounted.current) refreshOverview(); };
    window.addEventListener("focus", onFocus);
    unlisten.push(() => window.removeEventListener("focus", onFocus));
    load("settings.json", { defaults: {}, autoSave: true })
      .then(store => store.get<string>(RING_RELAY_SETTING))
      .then(v => { if (mounted.current && typeof v === "string" && v) { relayRef.current = v; setChosenRelay(v); } })
      .catch(() => {})
      .finally(() => { if (mounted.current) setRelayLoaded(true); });
    listen<PairingEvent>("ring:pairing", e => {
      const ev = e.payload;
      if (mounted.current && ev.flow === "phone" && operations.current.phone === ev.operation) setPairing(p => ({ ...p, phone: ev }));
    })
      .then(keep)
      .catch(() => {});
    listen<JoinEvent>("ring:join", e => { if (mounted.current) setJoinEvents(m => ({ ...m, [e.payload.operation]: e.payload })); })
      .then(keep)
      .catch(() => {});
    invoke<RingStatus>("ring_status").then(s => { if (mounted.current && s) setStatus(s); }).catch(() => {});
    refreshOverview();
    return () => {
      mounted.current = false;
      cancelled = true;
      unlisten.forEach(u => u());
    };
  }, []);

  // Registration/startup of the local Host emits no event of its own: re-read until ready.
  const notRegistered = overview?.unavailable === "not-registered";
  useEffect(() => {
    if (!notRegistered) return;
    const t = setInterval(() => {
      invoke<JoinOverview>("ring_join_overview").then(o => { if (mounted.current && o) setOverview(o); }).catch(() => {});
    }, 2000);
    return () => clearInterval(t);
  }, [notRegistered]);

  const apply = useCallback((s: RingStatus) => { if (mounted.current) setStatus(s); return s; }, []);

  // Only to start over after unreadable settings were set aside.
  const enable = useCallback(async (startOver: boolean) => {
    apply(await invoke<RingStatus>("ring_enable", { startOver }));
  }, [apply]);

  const setRelayUrl = useCallback(async (url: string) => {
    apply(await invoke<RingStatus>("ring_set_relay_url", { url }));
  }, [apply]);

  const claimHost = useCallback(async (host: string) => {
    apply(await invoke<RingStatus>("ring_claim_host", { host }));
  }, [apply]);

  // Removes a device (#22): a new Roster version without it.
  const removeMember = useCallback(async (signKey: string) => apply(await invoke<RingStatus>("ring_remove_member", { signKey })), [apply]);

  const startPhone = useCallback((operation: string) => {
    operations.current.phone = operation;
    return invoke<PhoneStart>("ring_pair_phone_start", { operation });
  }, []);
  const cancelPhone = useCallback((operation: string) => {
    if (operations.current.phone === operation) delete operations.current.phone;
    return invoke<void>("ring_pair_phone_cancel", { operation });
  }, []);
  // Add computer: the normalized eight words.
  const approveComputer = useCallback((words: string) => invoke<ApprovedComputer>("ring_approve_device_code", { code: words }), []);

  // The unpaired screen's Relay: `null` is the hosted one.
  const setUnpairedRelay = useCallback(async (url: string | null) => {
    setRelaySaving(true);
    try {
      const store = await load("settings.json", { defaults: {}, autoSave: true });
      await store.set(RING_RELAY_SETTING, url);
      // Committed: only now do Create and Join use it.
      relayRef.current = url;
      if (mounted.current) setChosenRelay(url);
    } finally {
      if (mounted.current) setRelaySaving(false);
    }
  }, []);
  const createComputer = useCallback(async () => { apply(await invoke<RingStatus>("ring_create_computer", { relayUrl: relayRef.current })); }, [apply]);
  const upgradeComputer = useCallback(async () => { apply(await invoke<RingStatus>("ring_upgrade_computer")); }, [apply]);
  const joinStart = useCallback((operation: string) => invoke<JoinStarted>("ring_join_start", { operation, relayUrl: relayRef.current }), []);
  const joinConfirm = useCallback((operation: string) => invoke<void>("ring_join_confirm", { operation }), []);
  const joinDecline = useCallback((operation: string) => invoke<void>("ring_join_decline", { operation }), []);
  const joinCancel = useCallback((operation: string) => invoke<void>("ring_join_cancel", { operation }), []);

  return {
    status, enable, setRelayUrl, claimHost, removeMember, pairing, startPhone, cancelPhone, approveComputer,
    joinEvents, overview, chosenRelay, relayReady: relayLoaded && !relaySaving, setUnpairedRelay, createComputer, upgradeComputer, joinStart, joinConfirm, joinDecline, joinCancel,
  };
}
