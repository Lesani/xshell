import { useCallback, useEffect, useRef, useState } from "react";
import { useModal } from "./useModal";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { X } from "lucide-react";
import { C, plural } from "../ring/copy";
import { errorText } from "../ring/members";

// The quit prompt (D5): closing the last xshell window while this machine runs terminals asks
// "Keep N terminals running?" — Keep running / Stop them, "Do not ask again" (Keep only). If
// terminals started meanwhile, a follow-up asks about those. The backend decides when to ask.
// Windows (W4, kind "close"): closing the window that runs this machine's terminals ends them,
// so it asks "Close N terminals on this machine?" — Close xshell / Cancel, nothing to remember.
// The count may be unknown (null): the question then has no number.

type Kind = "keep" | "close";

interface Prompt {
  id: string;
  kind: Kind;
  // Terminals the prompt counts (the follow-up: only the new ones); null: unknown ("close").
  n: number | null;
  more: boolean;
  error: { kind: "stop"; n: number } | null;
  pending: boolean;
}

type Reply = null | { changed: { id: string; added: number } };

export function QuitPrompt() {
  const [p, setP] = useState<Prompt | null>(null);
  const [remember, setRemember] = useState(false);
  const pRef = useRef<Prompt | null>(null);
  pRef.current = p;
  // The default button: Keep running, or Cancel for a prompt that would end terminals.
  const focusRef = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  // "Do not ask again" could not be saved: the backend dropped the prompt and the window stays
  // open with every terminal running; closing again asks again.
  const [settingFailed, setSettingFailed] = useState(false);
  useModal(panel, p !== null);

  // Listen first, then tell the backend it may ask.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    listen<{ id: string; terminals: number | null; kind?: Kind }>("local:quit-prompt", e => {
      const ev = e.payload;
      if (!ev || typeof ev.id !== "string") return;
      const kind: Kind = ev.kind === "close" ? "close" : "keep";
      const n = typeof ev.terminals === "number" ? ev.terminals : null;
      setRemember(false);
      setSettingFailed(false);
      setP({ id: ev.id, kind, n: kind === "keep" ? (n ?? 0) : n, more: false, error: null, pending: false });
    })
      .then(u => {
        if (cancelled) { u(); return; }
        unlisten = u;
        invoke("local_quit_prompt_ready").catch(() => {});
      })
      .catch(() => {});
    return () => { cancelled = true; unlisten?.(); };
  }, []);

  const resolve = useCallback(async (choice: "keep" | "stop" | "close" | "cancel") => {
    const cur = pRef.current;
    if (!cur || cur.pending) return;
    if (choice === "cancel") setP(null);
    else setP({ ...cur, pending: true });
    try {
      const reply = await invoke<Reply>("local_quit_resolve", { id: cur.id, choice, remember: choice === "keep" && !cur.more && remember });
      if (choice === "cancel") return;
      if (reply && reply.changed) {
        setP({ id: reply.changed.id, kind: "keep", n: reply.changed.added, more: true, error: null, pending: false });
        return;
      }
      // Done: the window closes.
      setP(null);
    } catch (e) {
      if (choice === "cancel") return;
      const text = errorText(e);
      const failed = /^failed:(\d+)/.exec(text);
      if (failed && Number(failed[1]) > 0) {
        // The same prompt stays answerable: Stop again, Keep running or Cancel.
        setP({ ...cur, pending: false, error: { kind: "stop", n: Number(failed[1]) } });
      } else if (failed) {
        setP(null);
        setSettingFailed(true);
      } else if (text.startsWith("busy")) {
        setP({ ...cur, pending: false });
      } else {
        // `stale`: the backend no longer holds this prompt; the window stays open.
        setP(null);
      }
    }
  }, [remember]);

  useEffect(() => {
    if (!p) return;
    focusRef.current?.focus();
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") { e.preventDefault(); void resolve("cancel"); } };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [p?.id, resolve]); // eslint-disable-line react-hooks/exhaustive-deps

  if (!p) {
    if (!settingFailed) return null;
    return (
      <div className="app-notice" role="alert" data-quit-setting-failed>
        <span className="app-notice-text">{C["quit.setting.failed"]}</span>
        <button className="app-notice-dismiss" onClick={() => setSettingFailed(false)} aria-label={C["quit.cancel"]}><X size={12} /></button>
      </div>
    );
  }
  if (p.kind === "close") {
    const title = p.n === null ? C["quit.close.title.unknown"] : plural("quit.close.title", p.n);
    const body = p.n === null ? C["quit.close.body.unknown"] : C["quit.close.body"];
    return (
      <div className="settings-overlay quit-prompt-overlay">
        <div ref={panel} className="settings-panel host-confirm-panel" role="dialog" aria-modal="true" aria-labelledby="quit-prompt-title" aria-describedby="quit-prompt-body" data-quit-prompt="close">
          <div className="settings-header">
            <span id="quit-prompt-title">{title}</span>
            <button className="settings-close" onClick={() => void resolve("cancel")} aria-label={C["quit.cancel"]}><X size={14} /></button>
          </div>
          <div className="settings-body">
            <p className="host-confirm-body" id="quit-prompt-body">{body}</p>
          </div>
          <div className="settings-footer">
            <button ref={focusRef} className="btn btn-ghost" data-quit-cancel disabled={p.pending} onClick={() => void resolve("cancel")}>{C["quit.cancel"]}</button>
            <span style={{ flex: 1 }} />
            <button className="btn btn-primary" data-quit-close disabled={p.pending} onClick={() => void resolve("close")}>{C["quit.close.confirm"]}</button>
          </div>
        </div>
      </div>
    );
  }
  const n = p.n ?? 0;
  const title = p.more ? C["quit.more.title"] : plural("quit.title", n);
  const body = p.more ? plural("quit.more", n) : C["quit.body"];
  return (
    <div className="settings-overlay quit-prompt-overlay">
      <div ref={panel} className="settings-panel host-confirm-panel" role="dialog" aria-modal="true" aria-labelledby="quit-prompt-title" aria-describedby="quit-prompt-body" data-quit-prompt={p.more ? "more" : "first"}>
        <div className="settings-header">
          <span id="quit-prompt-title">{title}</span>
          <button className="settings-close" onClick={() => void resolve("cancel")} aria-label={C["quit.cancel"]}><X size={14} /></button>
        </div>
        <div className="settings-body">
          <p className="host-confirm-body" id="quit-prompt-body">{body}</p>
          {!p.more && (
            <label className="quit-prompt-remember">
              <input type="checkbox" data-quit-remember checked={remember} disabled={p.pending} onChange={e => setRemember(e.target.checked)} />
              {C["quit.remember"]}
            </label>
          )}
          {p.error && (
            <p className="host-form-error" role="alert" data-quit-error>
              {plural("quit.failed", p.error.n)}
            </p>
          )}
        </div>
        <div className="settings-footer">
          <button className="btn btn-ghost" data-quit-cancel disabled={p.pending} onClick={() => void resolve("cancel")}>{C["quit.cancel"]}</button>
          <span style={{ flex: 1 }} />
          <button className="btn" data-quit-stop disabled={p.pending} onClick={() => void resolve("stop")}>{plural("quit.stop", n)}</button>
          <button ref={focusRef} className="btn btn-primary" data-quit-keep disabled={p.pending} onClick={() => void resolve("keep")}>{C["quit.keep"]}</button>
        </div>
      </div>
    </div>
  );
}
