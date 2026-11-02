// Per-launch Pi 1.4+ lifecycle reporting. No prompt, input, output or provider data is read.
import { VERSION } from "@earendil-works/pi-coding-agent";
import { spawn } from "node:child_process";

export default function (pi) {
  const version = typeof VERSION === "string" && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(VERSION);
  if (!version || version[1] !== "1" || Number(version[2]) < 4) return;
  const env = { ...process.env };
  const exe = env.XSHELL_EVENT_EXE;
  if (!exe || !env.XSHELL_EVENT_SOCKET || !env.XSHELL_TERMINAL_ID) return;
  let session;

  function stop(s) {
    if (!s) return;
    s.closed = true;
    clearTimeout(s.idle);
    clearTimeout(s.retry);
    s.cancel?.();
    s.wait?.();
    s.wait = undefined;
  }

  function attempt(s, status) {
    // An uncertain opposite-status send invalidates old acknowledgement knowledge.
    if (s.ack !== status) s.ack = undefined;
    return new Promise(resolve => {
      let child, timer, done = false;
      const finish = (ok, kill = false) => {
        if (done) return;
        done = true;
        clearTimeout(timer);
        if (kill) child?.kill("SIGKILL");
        child?.stdin?.destroy();
        child?.stdout?.destroy();
        child?.stderr?.destroy();
        s.cancel = undefined;
        resolve(ok);
      };
      s.cancel = () => finish(false, true);
      try {
        child = spawn(exe, ["event", "--strict", "-", status], {
          shell: false, env, stdio: "ignore", windowsHide: true,
        });
        child.once("error", () => finish(false));
        child.once("exit", code => finish(code === 0));
        timer = setTimeout(() => finish(false, true), 2500);
      } catch {
        finish(false);
      }
    });
  }

  async function pump(s) {
    if (s.closed || s.running) return;
    s.running = true;
    try {
      while (!s.closed && s.pending) {
        const revision = s.revision, status = s.desired;
        s.pending = false;
        const ok = await attempt(s, status);
        if (s.closed) break;
        if (ok) s.ack = status;
        if (revision !== s.revision) continue;
        s.wait?.();
        s.wait = undefined;
        if (!ok) {
          if (s.tries++ === 0) {
            s.retry = setTimeout(() => {
              s.retry = undefined;
              if (s.closed || revision !== s.revision) return;
              s.pending = true;
              void pump(s);
            }, 250);
          } else {
            try { s.ctx.ui.notify("xshell: Pi status reporting unavailable", "warning"); } catch { /* stale/no UI */ }
          }
        }
      }
    } finally {
      s.running = false;
    }
  }

  function desire(s, status) {
    if (s.closed) return Promise.resolve();
    clearTimeout(s.retry);
    s.retry = undefined;
    // Repeated lifecycle boundaries can resynchronize after exhausted delivery.
    if (s.desired === status && (s.pending || s.running)) return Promise.resolve();
    s.wait?.();
    s.wait = undefined;
    s.desired = status;
    s.revision++;
    s.tries = 0;
    if (s.ack === status) return Promise.resolve();
    s.pending = true;
    const firstAttempt = new Promise(resolve => { s.wait = resolve; });
    void pump(s);
    return firstAttempt;
  }

  function active(s) {
    if (s.closed || s.idle) return;
    s.idle = setTimeout(() => {
      s.idle = undefined;
      if (s.closed || s.desired !== "working") return;
      try {
        if (s.ctx.isIdle()) { void desire(s, "finished"); return; }
        active(s);
      } catch { stop(s); }
    }, 250);
  }

  pi.on("session_start", async (_event, ctx) => {
    stop(session);
    const s = session = { ctx, closed: false, running: false, pending: false, revision: 0, tries: 0 };
    try {
      const status = ctx.isIdle() ? "finished" : "working";
      const sent = desire(s, status);
      if (status === "working") active(s);
      await sent;
    } catch { stop(s); }
  });
  for (const event of ["before_agent_start", "agent_start", "session_before_compact"]) {
    pi.on(event, async () => {
      const s = session;
      if (!s || s.closed) return;
      const sent = desire(s, "working");
      active(s);
      await sent;
    });
  }
  // agent_end can be followed by retries, compaction or queued continuations.
  pi.on("agent_settled", async () => {
    const s = session;
    if (!s || s.closed) return;
    clearTimeout(s.idle);
    s.idle = undefined;
    await desire(s, "finished");
  });
  pi.on("session_shutdown", () => { stop(session); session = undefined; });
}
