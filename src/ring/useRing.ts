import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ApprovedComputer, JoinEvent, JoinOverview, JoinStarted, PairingEvent, PairingFlow, PhoneStart, RingStatus } from "./types";

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
  const mounted = useRef(false);
  const operations = useRef<Partial<Record<PairingFlow, string>>>({});

  useEffect(() => {
    mounted.current = true;
    const unlisten: (() => void)[] = [];
    let cancelled = false;
    const keep = (u: () => void) => { if (cancelled) u(); else unlisten.push(u); };
    listen<RingStatus>("ring:status", e => { if (mounted.current) setStatus(e.payload); })
      .then(keep)
      .catch(() => {});
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
    invoke<JoinOverview>("ring_join_overview").then(o => {
      if (!mounted.current || !o) return;
      setOverview(o);
      // Events seen meanwhile are newer than the overview.
      setJoinEvents(m => ({ ...Object.fromEntries(o.joins.map(j => [j.operation, j])), ...m }));
    }).catch(() => {});
    return () => {
      mounted.current = false;
      cancelled = true;
      unlisten.forEach(u => u());
    };
  }, []);

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

  const createComputer = useCallback(async () => { apply(await invoke<RingStatus>("ring_create_computer")); }, [apply]);
  const upgradeComputer = useCallback(async () => { apply(await invoke<RingStatus>("ring_upgrade_computer")); }, [apply]);
  const joinStart = useCallback((operation: string) => invoke<JoinStarted>("ring_join_start", { operation }), []);
  const joinConfirm = useCallback((operation: string) => invoke<void>("ring_join_confirm", { operation }), []);
  const joinDecline = useCallback((operation: string) => invoke<void>("ring_join_decline", { operation }), []);
  const joinCancel = useCallback((operation: string) => invoke<void>("ring_join_cancel", { operation }), []);

  return {
    status, enable, setRelayUrl, claimHost, removeMember, pairing, startPhone, cancelPhone, approveComputer,
    joinEvents, overview, createComputer, upgradeComputer, joinStart, joinConfirm, joinDecline, joinCancel,
  };
}
