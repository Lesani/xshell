import { useEffect, useRef } from "react";
import { Settings as SettingsIcon, Terminal as TerminalIcon } from "lucide-react";
import { AGENTS, AgentIcon, type AgentId } from "../agents";
import type { ProjectInfo } from "../types";
import { fmt } from "../hosts/strings";

// Project-scoped choice shown for Ask (including zero or one installed harness).
// Shell is a separate raw-Terminal action, never an AgentId or persisted default.
// Remote projects list their Host's installed harnesses.
export function AgentPickerDialog({ project, hostName, agents, onPick, onShell, onClose, onOpenSettings }: { project: ProjectInfo; hostName?: string; agents: AgentId[]; onPick: (agent: AgentId) => void; onShell: () => void; onClose: () => void; onOpenSettings: () => void }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) onClose(); };
    const handleKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => { document.removeEventListener("mousedown", handleClick); document.removeEventListener("keydown", handleKey); };
  }, [onClose]);

  return (
    <div className="picker-overlay">
      <div className="picker agent-picker" ref={ref}>
        <div className="picker-band"><span className="picker-band-label">New Chat</span></div>
        <div className="picker-head">
          <div className="picker-title">{hostName ? fmt("agentPicker.titleOnHost", { project: project.name, host: hostName }) : `Pick an agent for ${project.name}`}</div>
          <div className="picker-sub">Choose a coding agent or open a Shell in this Project.</div>
        </div>
        {agents.length === 0 && <div className="picker-empty">No coding agents detected{hostName ? ` on ${hostName}` : ""}. You can still open a Shell.</div>}
        <div className="agent-picker-options">
          {agents.map(id => (
            <button key={id} className="agent-picker-option" onClick={() => onPick(id)}>
              <span className={AGENTS[id].neutralIcon ? "agent-picker-neutral" : undefined}><AgentIcon agent={id} size={20} /></span>
              <span>{AGENTS[id].label}</span>
            </button>
          ))}
          <button className="agent-picker-option" onClick={onShell}>
            <TerminalIcon size={20} />
            <span>Shell</span>
          </button>
        </div>
        <div className="picker-footer">
          <span className="picker-footer-hint">Set a default agent to skip this dialog.</span>
          <button className="btn" onClick={onOpenSettings}><SettingsIcon size={11} /> Open Settings</button>
        </div>
      </div>
    </div>
  );
}
