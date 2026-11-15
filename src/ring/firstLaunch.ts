import type { LocalView } from "../local/localInfo";
import type { JoinOverview, RingStatus } from "./types";

// Which setup screen opens at start, if any (D9): `first` for a machine in no Ring that was not
// skipped, `old` after a format-1 Ring was set aside (until seen), `removed` after another device
// removed this machine. `old` and `removed` show even after a Skip.

export const SETUP_SKIPPED_SETTING = "ring_setup_skipped";
export const OLD_SEEN_SETTING = "ring_old_seen_at";

export type StartView = "first" | "old" | "removed";

export interface StartInputs {
  status: Pick<RingStatus, "membership" | "removed" | "oldRing"> | null | undefined;
  local: LocalView;
  overview?: Pick<JoinOverview, "unavailable"> | null;
  skipped: boolean;
  oldSeenAt: number | null;
}

export function startView({ status, local, overview, skipped, oldSeenAt }: StartInputs): StartView | null {
  if (!status || !status.membership || local === "failed") return null;
  if (overview?.unavailable === "platform") return null;
  if (status.membership === "member" || status.membership === "finishing") return null;
  if (status.membership === "removed") return "removed";
  if (status.oldRing && status.oldRing.at !== oldSeenAt) return "old";
  if (status.membership === "none" && !skipped) return "first";
  return null;
}
