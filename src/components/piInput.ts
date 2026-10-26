import type { Tab } from "../types";

/** xterm 5.5 collapses Shift+Enter to CR. Only explicit Pi Tabs opt into CSI-u here;
 * raw Shell agent metadata does not identify the program currently running in it.
 * Returns xterm's custom-key-handler decision (false suppresses the default CR).
 */
export function handlePiShiftEnter(
  tab: Pick<Tab, "agent" | "shellMode">,
  ev: KeyboardEvent,
  input: (data: string, wasUserInput: boolean) => void,
): boolean {
  if (tab.agent !== "pi" || tab.shellMode === "raw" || ev.type !== "keydown"
    || ev.key !== "Enter" || !ev.shiftKey || ev.altKey || ev.ctrlKey || ev.metaKey
    || ev.isComposing) return true;
  ev.preventDefault();
  input("\x1b[13;2u", true);
  return false;
}
