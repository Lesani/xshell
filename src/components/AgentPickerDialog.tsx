import { useEffect, useRef } from "react";
import { Settings as SettingsIcon } from "lucide-react";
import { AGENTS, AgentIcon, type AgentId } from "../agents";
import type { ProjectInfo } from "../types";
import { fmt } from "../hosts/strings";

// Shown when a new chat is requested while more than one agent CLI is installed and no
// default agent is set. The user picks per chat; the footer points at Settings → Agents
// where a default can be set so this dialog stops appearing. Never rendered for
// single-agent machines — those resolve silently to the one installed agent.
// `hostName` is set for a project on a Remote Host: the agents listed are the Host's CLIs.
export function AgentPickerDialog({ project, hostName, agents, onPick, onClose, onOpenSettings }: { project: ProjectInfo; hostName?: string; agents: AgentId[]; onPick: (agent: AgentId) => void; onClose: () => void; onOpenSettings: () => void }) {
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
          {agents.length > 0 && <div className="picker-sub">You have multiple coding agents on this system — choose which one hosts this chat.</div>}
        </div>
        {agents.length === 0 && hostName && <div className="picker-empty">{fmt("agentPicker.noneOnHost", { host: hostName })}</div>}
        <div className="agent-picker-options">
          {agents.map(id => (
            <button key={id} className="agent-picker-option" onClick={() => onPick(id)}>
              <span className={AGENTS[id].neutralIcon ? "agent-picker-neutral" : undefined}><AgentIcon agent={id} size={20} /></span>
              <span>{AGENTS[id].label}</span>
            </button>
          ))}
        </div>
        <div className="picker-footer">
          <span className="picker-footer-hint">Set a default agent to skip this dialog.</span>
          <button className="btn" onClick={onOpenSettings}><SettingsIcon size={11} /> Open Settings</button>
        </div>
      </div>
    </div>
  );
}
