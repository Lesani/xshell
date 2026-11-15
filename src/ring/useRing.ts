import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { load } from "@tauri-apps/plugin-store";
import type { ApprovedComputer, JoinEvent, JoinOverview, JoinStarted, PairingEvent, PairingFlow, PhoneStart, RingStatus } from "./types";

// The Relay chosen before the Ring exists (settings.json), bound into Create and Join. Absent:
// the hosted Relay.
export const RING_RELAY_SETTING = "ring_relay_url";

// The latest `ring:pairing` event of each flow (a new object per event).
export type PairingEvents = Record<PairingFlow, PairingEvent | null>;

// The Ring as Settings → Ring and the setup screen show it: `ring_status` once, then every
// `ring:status`; phone pairing (#9): its commands and every `ring:pairing`; Create, Join and
// Add a machine: their commands, `ring_join_overview` and every `ring:join`.
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

  const refreshOverview = useCallback(async (): Promise<JoinOverview | null> => {
    try {
      const o = await invoke<JoinOverview>("ring_join_overview");
      if (!mounted.current || !o) return o ?? null;
      setOverview(o);
      // Events seen meanwhile are newer than the overview.
      setJoinEvents(m => ({ ...Object.fromEntries((o.joins ?? []).map(j => [j.operation, j])), ...m }));
      return o;
    } catch {
      return null;
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    const unlisten: (() => void)[] = [];
    let cancelled = false;
    const keep = (u: () => void) => { if (cancelled) u(); else unlisten.push(u); };
    // Ownership can move between windows: every status change re-reads it.
    listen<RingStatus>("ring:status", e => { if (mounted.current) { setStatus(e.payload); void refreshOverview(); } })
      .then(keep)
      .catch(() => {});
    // An unpaired takeover need not change any status: re-read when the window gains focus.
    const onFocus = () => { if (mounted.current) void refreshOverview(); };
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
    void refreshOverview();
    return () => {
      mounted.current = false;
      cancelled = true;
      unlisten.forEach(u => u());
    };
  }, [refreshOverview]);

  const apply = useCallback((s: RingStatus) => { if (mounted.current && s) setStatus(s); return s; }, []);
  const refreshStatus = useCallback(async () => apply(await invoke<RingStatus>("ring_status")), [apply]);

  // Removes a device, this machine included: a new Roster version without its unit.
  const removeMember = useCallback(async (signKey: string) => apply(await invoke<RingStatus>("ring_remove_member", { signKey })), [apply]);

  const startPhone = useCallback((operation: string) => {
    operations.current.phone = operation;
    return invoke<PhoneStart>("ring_pair_phone_start", { operation });
  }, []);
  const cancelPhone = useCallback((operation: string) => {
    if (operations.current.phone === operation) delete operations.current.phone;
    return invoke<void>("ring_pair_phone_cancel", { operation });
  }, []);
  // Add a machine: the normalized eight words.
  const approveComputer = useCallback((words: string) => invoke<ApprovedComputer>("ring_approve_device_code", { code: words }), []);

  // The Relay chosen before the Ring exists: `null` is the hosted one.
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
  const createComputer = useCallback(async () => apply(await invoke<RingStatus>("ring_create_computer", { relayUrl: relayRef.current })), [apply]);
  const joinStart = useCallback((operation: string) => invoke<JoinStarted>("ring_join_start", { operation, relayUrl: relayRef.current }), []);
  const joinCancel = useCallback((operation: string) => invoke<void>("ring_join_cancel", { operation }), []);

  return {
    status, refreshStatus, removeMember, pairing, startPhone, cancelPhone, approveComputer,
    joinEvents, overview, refreshOverview, chosenRelay, relayReady: relayLoaded && !relaySaving, setUnpairedRelay, createComputer, joinStart, joinCancel,
  };
}

export type RingApi = ReturnType<typeof useRing>;
