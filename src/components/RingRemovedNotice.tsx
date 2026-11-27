import { X as XIcon } from "lucide-react";
import { C } from "../ring/copy";

// Another device removed this machine while xshell was open (U3): a notice with Join again,
// wherever the user is (Home, a Terminal, Settings).
export function RingRemovedNotice({ onJoin, onDismiss }: { onJoin: () => void; onDismiss: () => void }) {
  return (
    <div className="app-notice ring-removed-notice" role="alert" data-ring-removed-notice>
      <div className="app-notice-text">
        <div className="ring-removed-notice-title">{C["removed.title"]}</div>
        <div>{C["removed.body"]}</div>
      </div>
      <button className="btn btn-primary" data-ring-removed-join onClick={onJoin}>{C["removed.action"]}</button>
      <button className="btn btn-ghost" data-ring-removed-dismiss onClick={onDismiss}>{C["removed.dismiss"]}</button>
      <button className="app-notice-dismiss" onClick={onDismiss} aria-label={C["removed.dismiss"]}><XIcon size={12} /></button>
    </div>
  );
}
