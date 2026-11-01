import { useEffect, useId, useRef, useState } from "react";
import { fmt } from "../hosts/strings";
import { customTitleError } from "../tabs/customTitle";

interface Props {
  returnFocus: HTMLElement;
  seed: string;
  operation: "save" | "clear" | null;
  error: string | null;
  unavailable: string | null;
  hasCustomTitle: boolean;
  onSubmit: (name: string | null) => void;
  onClose: () => void;
}

export function RenameTabDialog({ returnFocus, seed, operation, error, unavailable, hasCustomTitle, onSubmit, onClose }: Props) {
  const [draft, setDraft] = useState(seed);
  const [validation, setValidation] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const form = useRef<HTMLFormElement>(null);
  const id = useId();
  useEffect(() => {
    const previous = returnFocus;
    input.current?.focus();
    input.current?.select();
    return () => { if (previous?.isConnected) previous.focus(); };
  }, []);
  const busy = operation !== null;
  useEffect(() => {
    if (!form.current?.contains(document.activeElement) || (document.activeElement as HTMLInputElement | null)?.disabled) {
      form.current?.querySelector<HTMLElement>("input:not(:disabled), button:not(:disabled)")?.focus();
    }
  }, [busy]);
  const message = validation || error || unavailable;
  return <div className="settings-overlay tab-rename-overlay" onPointerDown={e => { if (e.target === e.currentTarget) onClose(); }}>
    <form ref={form} className="settings-panel rename-tab-dialog" role="dialog" aria-modal="true" aria-labelledby={`${id}-title`} aria-describedby={`${id}-help`}
      onSubmit={e => { e.preventDefault(); if (busy || unavailable) return; const problem = customTitleError(draft); setValidation(problem); if (!problem) onSubmit(draft); }}
      onKeyDown={e => {
        e.stopPropagation();
        if (e.key === "Escape") { e.preventDefault(); onClose(); }
        if (e.key === "Tab") {
          const controls = Array.from(e.currentTarget.querySelectorAll<HTMLElement>("input:not(:disabled), button:not(:disabled)"));
          const first = controls[0], last = controls[controls.length - 1];
          if (!controls.includes(document.activeElement as HTMLElement)) { e.preventDefault(); first?.focus(); }
          else if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last?.focus(); }
          else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first?.focus(); }
        }
      }}>
      <h2 id={`${id}-title`}>{fmt("tab.rename.title")}</h2>
      <label htmlFor={`${id}-input`}>{fmt("tab.rename.label")}</label>
      <input id={`${id}-input`} ref={input} value={draft} disabled={busy} aria-invalid={!!validation} aria-describedby={message ? `${id}-error` : undefined} onChange={e => { setDraft(e.target.value); setValidation(null); }} />
      <p id={`${id}-help`}>{fmt("tab.rename.help")}</p>
      {message && <p id={`${id}-error`} role="alert">{message}</p>}
      {busy && <p role="status">{fmt(operation === "clear" ? "tab.rename.clearing" : "tab.rename.saving")}</p>}
      <div className="modal-actions">
        <button type="button" className="btn btn-ghost" disabled={busy || !!unavailable || !hasCustomTitle} onClick={() => { setValidation(null); onSubmit(null); }}>{fmt("tab.rename.automatic")}</button>
        <button type="button" className="btn btn-ghost" onClick={onClose}>{fmt("tab.rename.cancel")}</button>
        <button type="submit" className="btn btn-primary" disabled={busy || !!unavailable}>{fmt("tab.rename.save")}</button>
      </div>
    </form>
  </div>;
}
