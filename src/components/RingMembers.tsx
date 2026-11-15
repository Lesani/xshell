import { useEffect, useRef, useState } from "react";
import { Trash2 } from "lucide-react";
import { ConfirmDialog } from "./ConfirmDialog";
import { C, fmtc, type CopyKey } from "../ring/copy";
import { errorCode } from "../ring/setup";
import { errorText, hostOf, isMachine, isThisMachine, kindKey, presenceChip, removeDialog, removeErrorKey, versionLine } from "../ring/members";
import { MachineUpdateSlot, UpdateAllSlot } from "../ring/updateSlots";
import { useHostsSnapshot } from "../hosts/useHosts";
import { LOCAL_HOST } from "../hosts/localHost";
import type { MemberView, RingStatus } from "../ring/types";

// Settings → Ring → Your devices: one row per machine or phone, Remove on every row (this
// machine's included), "Update available" per machine and "Update all" (slice S4's seam).

type Removal =
  | { kind: "idle" }
  | { kind: "confirming"; member: MemberView }
  | { kind: "removing"; member: MemberView }
  | { kind: "failed"; member: MemberView; key: CopyKey }
  | { kind: "done"; name: string };

export interface RingMembersProps {
  s: RingStatus;
  // Another xshell window runs the connection: removing is refused here.
  readOnly: boolean;
  remove: (signKey: string) => Promise<RingStatus>;
  // Re-reads the status (Retry after an uncertain removal).
  refresh: () => Promise<RingStatus>;
  // This machine left the Ring by its own Remove.
  onSelfRemoved: () => void;
}

const chipClass = (key: CopyKey) => (key === "member.online" ? "connected" : "offline");

export function RingMembers({ s, readOnly, remove, refresh, onSelfRemoved }: RingMembersProps) {
  const snap = useHostsSnapshot();
  const [removal, setRemoval] = useState<Removal>({ kind: "idle" });
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);

  const run = async (m: MemberView) => {
    setRemoval({ kind: "removing", member: m });
    try {
      await remove(m.signKey);
      if (isThisMachine(m)) { onSelfRemoved(); return; }
      if (mounted.current) setRemoval({ kind: "done", name: m.name });
    } catch (e) {
      if (mounted.current) setRemoval({ kind: "failed", member: m, key: removeErrorKey(errorCode(errorText(e))) });
    }
  };
  // Retry: membership is read first; a device already gone is not removed again.
  const retry = async (m: MemberView) => {
    setRemoval({ kind: "removing", member: m });
    let fresh: RingStatus | null = null;
    try { fresh = await refresh(); } catch { /* the removal reports the connection */ }
    if (fresh && !fresh.members.some(x => x.signKey === m.signKey)) {
      if (isThisMachine(m)) { onSelfRemoved(); return; }
      if (mounted.current) setRemoval({ kind: "done", name: m.name });
      return;
    }
    await run(m);
  };

  const dialog = removal.kind === "confirming" ? removeDialog(removal.member, s.members) : null;
  return (
    <div className="edit-field" data-ring-members>
      <label className="edit-label">{C["ring.members"]}</label>
      <UpdateAllSlot />
      {s.members.map(m => {
        const self = isThisMachine(m);
        const hostId = self ? LOCAL_HOST : hostOf(m, snap.available);
        const status = hostId ? snap.status[hostId] : undefined;
        const chip = presenceChip(m, s.connection);
        const version = versionLine(m, status);
        const removing = removal.kind === "removing" && removal.member.signKey === m.signKey;
        return (
          <div key={m.signKey} className="host-row" data-ring-member={m.signKey}>
            <div className="host-row-head">
              <div className="host-row-title">
                <span className="host-row-name">{m.name}</span>
                <span className="host-row-target">
                  {fmtc(kindKey(m))}
                  {self ? ` · ${C["member.this"]}` : ""}
                </span>
                {version && <span className="host-row-muted">{version}</span>}
              </div>
              {chip && <span className={`host-chip host-chip-${chipClass(chip)}`}><span className="host-chip-dot" />{fmtc(chip)}</span>}
            </div>
            {isMachine(m) && !self && hostId && <MachineUpdateSlot host={{ id: hostId, name: m.name }} status={status} />}
            <div className="host-row-actions">
              <span style={{ flex: 1 }} />
              <button className="btn btn-ghost settings-action-btn host-row-remove" data-member-remove={m.signKey} aria-label={fmtc("member.removeLabel", { name: m.name })}
                disabled={readOnly || removal.kind === "removing" || s.busy} onClick={() => setRemoval({ kind: "confirming", member: m })}>
                <Trash2 size={11} /> {removing ? C["remove.progress"] : C["member.remove"]}
              </button>
            </div>
          </div>
        );
      })}
      <div aria-live="polite">
        {removal.kind === "done" && <div className="host-row-note" data-remove-done>{fmtc("remove.done", { name: removal.name })}</div>}
        {removal.kind === "failed" && (
          <div className="host-form-error" role="alert" data-remove-error>
            {fmtc(removal.key)}
            {removal.key === "remove.uncertain" && (
              <div className="host-row-actions">
                <button className="btn settings-action-btn" data-remove-retry onClick={() => void retry(removal.member)}>{C["remove.retry"]}</button>
              </div>
            )}
          </div>
        )}
      </div>
      {dialog && removal.kind === "confirming" && (
        <ConfirmDialog title={dialog.title} body={dialog.body} confirm={C["remove.action"]} cancel={C["remove.cancel"]}
          onConfirm={() => void run(removal.member)} onCancel={() => setRemoval({ kind: "idle" })} />
      )}
    </div>
  );
}
