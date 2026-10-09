import type { TabAgentStatus } from "../tabs/agentStatus";

// A Tab's Agent Status as a small dot: working pulses, needs you is amber, finished green,
// ended a grey ring; a stale (last known) status is dimmed.
export function AgentStatusBadge({ value, label }: { value: TabAgentStatus; label: string }) {
  const cls = `tab-agent-status tab-agent-status-${value.status}${value.stale ? " tab-agent-status-stale" : ""}`;
  return <span className={cls} role="img" aria-label={label} />;
}
