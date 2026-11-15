import { useEffect, useRef, useState } from "react";
import { useModal } from "./useModal";
import { C, fmtc } from "../ring/copy";
import { localView, logPath, type LocalStatus } from "../local/localInfo";

// Terminals cannot start on this machine (the Daemon failed): a blocking view with the log path
// and Retry, never a fallback to Terminals inside the app. The A10 variant: an older xshell's
// window still runs this machine's Terminals.

export function LocalFailedView({ info, retry }: { info: LocalStatus | null; retry: () => Promise<unknown> }) {
  const [pending, setPending] = useState(false);
  const mounted = useRef(true);
  const retryRef = useRef<HTMLButtonElement>(null);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const failed = localView(info) === "failed";
  const panel = useRef<HTMLDivElement>(null);
  useEffect(() => { if (failed) retryRef.current?.focus(); }, [failed]);
  useModal(panel, failed);
  if (!failed) return null;
  const older = info?.daemon?.errorKind === "older-desktop";
  const run = async () => {
    if (pending) return;
    setPending(true);
    try { await retry(); } catch { /* the view stays; the next event tells */ } finally { if (mounted.current) setPending(false); }
  };
  return (
    <div className="local-failed-overlay">
      <div ref={panel} className="local-failed-panel" role="alertdialog" aria-modal="true" aria-labelledby="local-failed-title" aria-describedby="local-failed-body" data-local-failed={older ? "older" : "failed"}>
        <h1 id="local-failed-title" className="local-failed-title">{older ? C["local.older.title"] : C["local.failed.title"]}</h1>
        {older ? (
          <p id="local-failed-body" className="local-failed-body">{C["local.older.body"]}</p>
        ) : (
          <p id="local-failed-body" className="local-failed-body">
            {splitPath(fmtc("local.failed.body", { path: "\u0000" }), logPath(info))}
          </p>
        )}
        <div className="local-failed-actions">
          <button ref={retryRef} className="btn btn-primary" data-local-retry disabled={pending} onClick={() => void run()}>{C["local.retry"]}</button>
        </div>
      </div>
    </div>
  );
}

// The body with the path as a selectable code span.
function splitPath(text: string, path: string) {
  const [before, after = ""] = text.split("\u0000");
  return <>{before}<code className="local-failed-path" data-local-log>{path}</code>{after}</>;
}
