import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

// Conservative until native focus/minimized queries complete. Native focus, not DOM focus,
// determines whether the Desktop can actually observe a visible Terminal.
export function useDesktopForeground() {
  const [foreground, setForeground] = useState(false);
  const current = useRef(false);
  const isForeground = () => current.current;
  useEffect(() => {
    let live = true, revision = 0;
    let unlisten: (() => void) | undefined;
    const win = getCurrentWindow();
    const apply = (value: boolean) => { current.current = value; setForeground(value); };
    const refresh = async () => {
      const rev = ++revision;
      try {
        const [focused, minimized] = await Promise.all([win.isFocused(), win.isMinimized()]);
        if (live && rev === revision) apply(focused && !minimized && document.visibilityState === "visible");
      } catch { if (live && rev === revision) apply(false); }
    };
    const visibility = () => {
      if (document.visibilityState !== "visible") { revision++; apply(false); }
      else void refresh();
    };
    // Subscribe before querying so a focus loss during startup cannot be overwritten.
    try {
      void win.onFocusChanged(e => {
        if (!e.payload) { revision++; if (live) apply(false); }
        else void refresh();
      }).then(off => { if (live) unlisten = off; else off(); }).then(refresh).catch(() => {});
    } catch { /* Native window API unavailable: never infer foreground from DOM alone. */ }
    document.addEventListener("visibilitychange", visibility);
    return () => { live = false; current.current = false; revision++; unlisten?.(); document.removeEventListener("visibilitychange", visibility); };
  }, []);
  return { foreground, isForeground };
}
