import { outcomeMs } from "../ring/outcome";
import { useEffect, useId, useRef, useState } from "react";
import { Monitor } from "lucide-react";
import { C, fmtc, type CopyKey } from "../ring/copy";
import { errorCode } from "../ring/setup";
import { errorText } from "../ring/members";
import { normalizeWords, validWords, WORD_COUNT } from "../ring/pairing";
import type { ApprovedComputer } from "../ring/types";

// Settings → Ring → Add a machine: the eight words the other machine shows. One request
// (`ring_approve_device_code`); the machine is added at once, nothing to confirm there.

const APPROVE_ERRORS: Record<string, CopyKey> = {
  invalid_code: "add.invalid",
  not_found: "add.invalid",
  expired: "add.invalid",
  already_used: "add.alreadyUsed",
  offline: "add.offline",
  closed: "add.closed",
  busy: "add.busy",
  not_approver: "add.notApprover",
  finishing: "add.finishing",
  other_window: "add.otherWindow",
};

export function approveErrorKey(code: string): CopyKey {
  return APPROVE_ERRORS[code] ?? "add.failed";
}

// The local format check: shown once the input cannot become eight words by typing on, or
// after a submit attempt.
export function formatError(raw: string, attempted: boolean): boolean {
  const w = normalizeWords(raw).split("-").filter(Boolean);
  if (w.length === 0 || validWords(raw)) return false;
  return attempted || w.length > WORD_COUNT || w.some(x => !/^[a-z]+$/.test(x));
}

type AddState = { kind: "idle" } | { kind: "checking" } | { kind: "done"; names: string[] } | { kind: "failed"; key: CopyKey };

export function AddMachinePanel({ approve, disabled = false, note = null }: { approve: (words: string) => Promise<ApprovedComputer>; disabled?: boolean; note?: string | null }) {
  const [words, setWords] = useState("");
  const [attempted, setAttempted] = useState(false);
  const [st, setSt] = useState<AddState>({ kind: "idle" });
  const busy = useRef(false);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  // An outcome line describes the last action only: it goes away by itself.
  useEffect(() => {
    if (st.kind !== "done") return;
    const t = window.setTimeout(() => setSt(v => (v.kind === "done" ? { kind: "idle" } : v)), outcomeMs());
    return () => window.clearTimeout(t);
  }, [st]);
  const inputId = useId();
  const noteId = useId();

  const submit = async () => {
    if (busy.current || disabled) return;
    if (!validWords(words)) { setAttempted(true); return; }
    busy.current = true;
    setSt({ kind: "checking" });
    try {
      const added = await approve(normalizeWords(words));
      if (!mounted.current) return;
      setSt({ kind: "done", names: added.names });
      setWords("");
      setAttempted(false);
    } catch (e) {
      if (mounted.current) setSt({ kind: "failed", key: approveErrorKey(errorCode(errorText(e))) });
    } finally {
      busy.current = false;
    }
  };

  const checking = st.kind === "checking";
  const invalid = formatError(words, attempted);
  return (
    <div className="edit-field" data-add-machine>
      <label className="edit-label">{C["add.entry"]}</label>
      {note && <div className="host-row-note host-row-warn" data-ring-connection-required>{note}</div>}
      <div className="edit-hint">{C["add.body"]}</div>
      <div className="edit-field">
        <label className="edit-label" htmlFor={inputId}>{C["add.label"]}</label>
        <input id={inputId} className="edit-input host-form-mono" value={words} placeholder={C["add.placeholder"]} spellCheck={false}
          autoCapitalize="none" autoComplete="off" disabled={checking || disabled} data-computer-words aria-describedby={noteId} aria-invalid={invalid || undefined}
          onChange={(e) => { setWords(e.target.value); if (st.kind !== "checking") setSt({ kind: "idle" }); }}
          onKeyDown={(e) => { if (e.key === "Enter") void submit(); }} />
        {invalid && <div className="host-form-error">{C["add.format"]}</div>}
      </div>
      <div className="host-row-actions">
        <button className="btn btn-primary settings-action-btn" data-add-computer disabled={checking || disabled || !validWords(words)} onClick={() => void submit()}>
          <Monitor size={11} /> {C["add.action"]}
        </button>
      </div>
      <div id={noteId} aria-live="polite">
        {st.kind === "checking" && <div className="host-row-note">{C["add.checking"]}</div>}
        {st.kind === "done" && <div className="host-row-note" data-add-done>{fmtc("add.done", { name: [...new Set(st.names)].join(", ") })}</div>}
        {st.kind === "failed" && <div className="host-form-error" role="alert">{fmtc(st.key)}</div>}
      </div>
    </div>
  );
}
