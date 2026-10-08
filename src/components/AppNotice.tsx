import { useEffect } from "react";
import { AlertTriangle, X as XIcon } from "lucide-react";
import { fmt } from "../hosts/strings";

const AUTO_HIDE_MS = 6000;

// Dismissible app-wide banner (e.g. a refused new Terminal on an offline Host). Hides itself
// after 6 s; a new notice restarts the timer.
export function AppNotice({ notice, onDismiss }: { notice: { id: number; text: string } | null; onDismiss: () => void }) {
  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(onDismiss, AUTO_HIDE_MS);
    return () => window.clearTimeout(t);
  }, [notice, onDismiss]);
  if (!notice) return null;
  return (
    <div className="app-notice" role="status">
      <AlertTriangle size={13} className="app-notice-icon" />
      <span className="app-notice-text">{notice.text}</span>
      <button className="app-notice-dismiss" onClick={onDismiss} aria-label={fmt("notice.dismiss")} title={fmt("notice.dismiss")}><XIcon size={12} /></button>
    </div>
  );
}
