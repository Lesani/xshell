import { useEffect } from "react";
import { X } from "lucide-react";
import { fmt } from "../hosts/strings";

// A modal yes/no for Settings → Hosts: Escape and a click outside cancel.
export function ConfirmDialog({ title, body, confirm, onConfirm, onCancel }: { title: string; body: string; confirm: string; onConfirm: () => void; onCancel: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onCancel(); };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCancel]);
  return (
    <div className="settings-overlay" onClick={(e) => { if (e.target === e.currentTarget) onCancel(); }}>
      <div className="settings-panel host-confirm-panel">
        <div className="settings-header"><span>{title}</span><button className="settings-close" onClick={onCancel} aria-label={fmt("hosts.form.cancel")}><X size={14} /></button></div>
        <div className="settings-body"><p className="host-confirm-body">{body}</p></div>
        <div className="settings-footer">
          <button className="btn btn-ghost" onClick={onCancel}>{fmt("hosts.form.cancel")}</button>
          <button className="btn btn-primary" onClick={onConfirm}>{confirm}</button>
        </div>
      </div>
    </div>
  );
}
