import { useEffect, useId, useRef } from "react";
import { X } from "lucide-react";
import { fmt } from "../hosts/strings";
import { useModal } from "./useModal";

// A modal yes/no for Settings: Escape and a click outside cancel; focus starts on Cancel, stays
// inside while open and returns where it was.
export function ConfirmDialog({ title, body, confirm, cancel, onConfirm, onCancel }: { title: string; body: string; confirm: string; cancel?: string; onConfirm: () => void; onCancel: () => void }) {
  const titleId = useId();
  const bodyId = useId();
  const cancelRef = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onCancel(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);
  useEffect(() => { cancelRef.current?.focus(); }, []);
  useModal(panel, true);
  return (
    <div className="settings-overlay" onClick={(e) => { if (e.target === e.currentTarget) onCancel(); }}>
      <div ref={panel} className="settings-panel host-confirm-panel" role="dialog" aria-modal="true" aria-labelledby={titleId} aria-describedby={bodyId}>
        <div className="settings-header"><span id={titleId}>{title}</span><button className="settings-close" onClick={onCancel} aria-label={cancel ?? fmt("hosts.form.cancel")}><X size={14} /></button></div>
        <div className="settings-body"><p className="host-confirm-body" id={bodyId}>{body}</p></div>
        <div className="settings-footer">
          <button ref={cancelRef} className="btn btn-ghost" onClick={onCancel}>{cancel ?? fmt("hosts.form.cancel")}</button>
          <button className="btn btn-primary" onClick={onConfirm}>{confirm}</button>
        </div>
      </div>
    </div>
  );
}
