import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { PairingEvent, PairingFlow, PhoneStart, RingStatus } from "./types";

// The latest `ring:pairing` event of each flow (a new object per event).
export type PairingEvents = Record<PairingFlow, PairingEvent | null>;

// The Ring as Settings → Mobile shows it: `ring_status` once, then every `ring:status`; and
// pairing (#9): its commands and every `ring:pairing`.
export function useRing() {
  const [status, setStatus] = useState<RingStatus | null>(null);
  const [pairing, setPairing] = useState<PairingEvents>({ phone: null, computer: null });
  const mounted = useRef(false);

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
      if (mounted.current && (ev.flow === "phone" || ev.flow === "computer")) setPairing(p => ({ ...p, [ev.flow]: ev }));
    })
      .then(keep)
      .catch(() => {});
    invoke<RingStatus>("ring_status").then(s => { if (mounted.current) setStatus(s); }).catch(() => {});
    return () => {
      mounted.current = false;
      cancelled = true;
      unlisten.forEach(u => u());
    };
  }, []);

  const enable = useCallback(async (startOver: boolean) => {
    const s = await invoke<RingStatus>("ring_enable", { startOver });
    if (mounted.current) setStatus(s);
  }, []);

  const setRelayUrl = useCallback(async (url: string) => {
    const s = await invoke<RingStatus>("ring_set_relay_url", { url });
    if (mounted.current) setStatus(s);
  }, []);

  const claimHost = useCallback(async (host: string) => {
    const s = await invoke<RingStatus>("ring_claim_host", { host });
    if (mounted.current) setStatus(s);
  }, []);

  // Removes a device (#22): a new Roster version without it.
  const removeMember = useCallback(async (signKey: string) => {
    const s = await invoke<RingStatus>("ring_remove_member", { signKey });
    if (mounted.current) setStatus(s);
    return s;
  }, []);

  const startPhone = useCallback(() => invoke<PhoneStart>("ring_pair_phone_start"), []);
  const cancelPhone = useCallback(() => invoke<void>("ring_pair_phone_cancel"), []);
  const pairComputer = useCallback((code: string) => invoke<void>("ring_pair_computer", { code }), []);
  const cancelComputer = useCallback(() => invoke<void>("ring_pair_cancel"), []);

  return { status, enable, setRelayUrl, claimHost, removeMember, pairing, startPhone, cancelPhone, pairComputer, cancelComputer };
}
