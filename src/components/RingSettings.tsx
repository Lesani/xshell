import { useEffect, useId, useState } from "react";
import { C } from "../ring/copy";
import { fmt } from "../ring/strings";
import { connectionLine, localLine, problemLine, quotaLine, relayUrlError, validRelayUrl } from "../ring/mobileSettings";
import { useRing, type RingApi } from "../ring/useRing";
import { AddMachinePanel } from "./AddMachinePanel";
import { PhonePanel, type Pairing } from "./PhonePanel";
import { RingMembers } from "./RingMembers";
import type { RingStatus } from "../ring/types";

// Settings → Ring: set up the Ring (the setup screen), or, as a member, its devices with Remove
// on every row, Add a machine (eight words) and Pair a phone. No consent, upgrade, leave or
// Relay move.

export interface RingSettingsProps {
  // Settings is shown on Ring (a phone code dies when it is hidden).
  visible?: boolean;
  // Opens the setup screen; `join` starts the eight words at once.
  onOpenRingSetup: (initial?: "join") => void;
  ring?: RingApi;
}

export function RingSettings(props: RingSettingsProps) {
  const own = useRing();
  return <RingSettingsView {...props} ring={props.ring ?? own} />;
}

// A member's Relay is fixed: shown, not edited.
function FixedRelay({ s }: { s: RingStatus }) {
  return (
    <div className="edit-field" data-ring-relay-fixed>
      <label className="edit-label">{fmt("mobile.relay.title")}</label>
      <div className="host-row-note">{s.relayHosted ? fmt("mobile.relay.hosted") : (s.relayUrl ?? "")}</div>
    </div>
  );
}

// The self-hosted Relay, chosen before the Ring is created (advanced). Empty: the hosted Relay.
function UnpairedRelay({ chosen, onSave, disabled }: { chosen: string | null; onSave: (url: string | null) => Promise<void>; disabled: boolean }) {
  const [url, setUrl] = useState(chosen ?? "");
  const [saving, setSaving] = useState(false);
  const id = useId();
  useEffect(() => { setUrl(chosen ?? ""); }, [chosen]);
  const trimmed = url.trim();
  const dirty = trimmed !== (chosen ?? "");
  const ok = trimmed === "" || validRelayUrl(trimmed);
  const error = relayUrlError(url);
  const save = async () => {
    if (!dirty || !ok || saving || disabled) return;
    setSaving(true);
    try { await onSave(trimmed === "" ? null : trimmed); } catch { /* the field keeps the value */ } finally { setSaving(false); }
  };
  return (
    <div className="edit-field" data-ring-relay-unpaired>
      <label className="edit-label" htmlFor={id}>{C["ring.relay.label"]}</label>
      <input id={id} className="edit-input host-form-mono" value={url} placeholder={fmt("mobile.relay.url.placeholder")} spellCheck={false} disabled={disabled}
        onChange={(e) => setUrl(e.target.value)} onKeyDown={(e) => { if (e.key === "Enter") void save(); }} />
      {error ? <div className="host-form-error">{error}</div> : <div className="edit-hint">{C["ring.relay.help"]}</div>}
      <div className="host-row-actions">
        <button className="btn settings-action-btn" disabled={!dirty || !ok || saving || disabled} onClick={() => void save()}>{fmt("mobile.relay.save")}</button>
      </div>
    </div>
  );
}

export function RingSettingsView({ visible = true, onOpenRingSetup, ring: r }: RingSettingsProps & { ring: RingApi }) {
  const s = r.status;
  // This machine removed itself: the line stays until the next Ring change here.
  const [selfRemoved, setSelfRemoved] = useState(false);
  useEffect(() => { if (s?.membership === "member" || s?.membership === "finishing") setSelfRemoved(false); }, [s?.membership]);
  if (!s) return null;
  const follower = s.connection === "other-window" || r.overview?.owner === false;
  const problem = problemLine(s);
  const local = localLine(s);
  const platform = r.overview?.unavailable === "platform";
  const p: Pairing = { events: r.pairing, startPhone: r.startPhone, cancelPhone: r.cancelPhone };
  const conn = connectionLine(s);
  const quota = quotaLine(s, Date.now());
  return (
    <div className="settings-section" data-ring-section>
      <div className="settings-section-head">
        <div className="settings-section-title">{C["ring.title"]}</div>
        <div className="settings-section-desc">{C["ring.body"]}</div>
      </div>
      <div className="settings-section-body">
        {problem && <div className="host-row-note host-row-warn">{problem}</div>}
        {s.membership === "finishing" ? (
          <div className="edit-field" data-ring-finishing>
            <label className="edit-label">{C["finish.title"]}</label>
            <div className="host-row-note">{s.finishingOffline ? C["finish.offline"] : C["finish.body"]}</div>
          </div>
        ) : s.membership === "member" ? (
          <>
            <FixedRelay s={s} />
            {conn && <div className="host-row-note">{conn}</div>}
            {quota && <div className="host-row-note host-row-warn">{quota}</div>}
            <RingMembers s={s} readOnly={follower} remove={r.removeMember} refresh={r.refreshStatus} onSelfRemoved={() => setSelfRemoved(true)} />
            {follower ? <div className="host-row-note">{C["ring.follower"]}</div> : (
              <>
                <AddMachinePanel approve={r.approveComputer} disabled={s.busy} />
                <PhonePanel s={s} p={p} key={visible ? "shown" : "hidden"} />
              </>
            )}
          </>
        ) : (
          <>
            {selfRemoved && <div className="host-row-note" data-ring-self-removed>{C["remove.self.done"]}</div>}
            {s.membership === "removed" && !selfRemoved && (
              <div className="edit-field" data-ring-removed>
                <label className="edit-label">{C["removed.title"]}</label>
                <div className="host-row-note">{C["removed.body"]}</div>
                {!follower && (
                  <div className="host-row-actions">
                    <button className="btn btn-primary settings-action-btn" data-ring-join-again disabled={s.busy || platform} onClick={() => onOpenRingSetup("join")}>{C["removed.action"]}</button>
                  </div>
                )}
              </div>
            )}
            {s.oldRing && s.membership !== "removed" && (
              <div className="edit-field" data-ring-old>
                <label className="edit-label">{C["old.title"]}</label>
                <div className="host-row-note">{C["old.body"]}</div>
                <div className="host-row-note">{C["old.local"]}</div>
              </div>
            )}
            {follower ? <div className="host-row-note">{C["ring.follower"]}</div> : (
              <>
                <div className="host-row-actions">
                  <button className="btn btn-primary settings-action-btn" data-ring-setup-open disabled={platform || s.problem === "unreadable"} onClick={() => onOpenRingSetup()}>{C["first.settings"]}</button>
                </div>
                {/* COPY-GAP: no Desktop text for "not available on this platform" (Windows, Stage 2). */}
                {platform && <div className="host-row-note host-row-warn">{fmt("ring.unsupported")}</div>}
                <UnpairedRelay chosen={r.chosenRelay} onSave={r.setUnpairedRelay} disabled={!r.relayReady || s.busy} />
              </>
            )}
          </>
        )}
        {local && <div className="host-row-note">{local}</div>}
      </div>
    </div>
  );
}
