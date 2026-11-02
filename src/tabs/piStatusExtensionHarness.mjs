import assert from "node:assert/strict";
import { SourceTextModule, SyntheticModule, createContext } from "node:vm";
import { readFile, writeFile, mkdtemp, chmod, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EventEmitter } from "node:events";
import { spawn as realSpawn } from "node:child_process";

const bytes = await readFile(new URL("../../src-tauri/crates/core/src/pi_status.js", import.meta.url), "utf8");
const flush = async () => { for (let i = 0; i < 15; i++) await Promise.resolve(); };
async function harness({ version = "1.4.0", real = false, exe = "/helper 'path;&", extraEnv = {} } = {}) {
  const handlers = new Map(), calls = [], warnings = [], children = [], timers = new Map();
  let now = 0, next = 1, idle = true, outcomes = [], throwsIdle = false;
  const ctx = { isIdle() { if (throwsIdle) throw new Error("invalidated"); return idle; }, ui: { notify: (...args) => warnings.push(args) } };
  function spawn(exe, args, options) {
    const call = { exe, args, options }; calls.push(call);
    if (real) {
      const child = realSpawn(exe, args, options); children.push(child); return child;
    }
    const outcome = outcomes.shift() ?? 0;
    if (outcome === "throw") throw new Error("private stderr never displayed");
    const child = new EventEmitter(); children.push(child);
    child.kill = signal => { child.killed = signal; return true; };
    child.stdin = child.stdout = child.stderr = { destroy() { child.destroyed = true; } };
    if (outcome !== "hang") queueMicrotask(() => outcome === "error" ? child.emit("error", new Error("secret")) : child.emit("exit", outcome));
    call.child = child;
    return child;
  }
  const env = { XSHELL_EVENT_EXE: exe, XSHELL_EVENT_SOCKET: "/private/socket", XSHELL_TERMINAL_ID: "uuid.7", ...extraEnv };
  const context = createContext({
    process: { env },
    setTimeout: real ? setTimeout : (fn, delay) => { const id = next++; timers.set(id, { fn, at: now + delay }); return id; },
    clearTimeout: real ? clearTimeout : id => timers.delete(id),
  });
  const module = new SourceTextModule(bytes, { context });
  await module.link(name => {
    const values = name === "node:child_process" ? { spawn } : { VERSION: version };
    return new SyntheticModule(Object.keys(values), function () { for (const [k, v] of Object.entries(values)) this.setExport(k, v); }, { context });
  });
  await module.evaluate();
  module.namespace.default({ on: (event, fn) => handlers.set(event, fn) });
  async function emit(event, payload = {}) { await handlers.get(event)?.(payload, ctx); await flush(); }
  async function tick(ms) {
    const end = now + ms;
    while (true) {
      const due = [...timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      now = due[1].at; timers.delete(due[0]); due[1].fn(); await flush();
    }
    now = end; await flush();
  }
  return { handlers, calls, warnings, children, timers, env, ctx, emit, tick,
    idle: value => { idle = value; }, outcomes: (...values) => { outcomes = values; }, invalidate: () => { throwsIdle = true; },
    statuses: () => calls.map(c => c.args.at(-1)) };
}
const scenario = process.argv[2];
if (scenario === "compatibility") {
  for (const version of [undefined, null, "", "1.3.9", "2.4.0", "1.4.0-rc.1", "1.4", "v1.4.0", "01.4.0"]) {
    const h = await harness({ version: version === undefined ? "bad" : version });
    assert.equal(h.handlers.size, 0); assert.equal(h.timers.size, 0); assert.equal(h.calls.length, 0);
  }
  for (const version of ["1.4.0", "1.5.99"]) {
    const h = await harness({ version }); assert.equal(h.calls.length, 0); assert.equal(h.timers.size, 0);
    await h.emit("session_start"); assert.deepEqual(h.statuses(), ["finished"]); await h.emit("session_shutdown");
  }
  const missing = await harness({ extraEnv: { XSHELL_EVENT_EXE: "" } }); assert.equal(missing.handlers.size, 0);
} else if (scenario === "lifecycle") {
  const h = await harness(); await h.emit("session_start");
  h.idle(false); await h.emit("before_agent_start");
  for (const event of ["agent_start", "agent_end", "agent_start", "agent_end", "turn_end", "tool_execution_start", "message_update", "input", "ui_prompt_start"]) await h.emit(event);
  await h.tick(1000); assert.deepEqual(h.statuses(), ["finished", "working"]);
  assert.equal(h.timers.size, 1);
  h.idle(true); await h.emit("agent_settled", { aborted: true });
  assert.deepEqual(h.statuses(), ["finished", "working", "finished"]); assert.equal(h.timers.size, 0);
  await h.emit("agent_settled"); assert.equal(h.calls.length, 3);
  const call = h.calls[1]; assert.equal(call.exe, "/helper 'path;&");
  assert.deepEqual(Array.from(call.args), ["event", "--strict", "-", "working"]);
  assert.equal(call.options.shell, false); assert.equal(call.options.stdio, "ignore");
  h.env.XSHELL_TERMINAL_ID = "mutated"; assert.equal(call.options.env.XSHELL_TERMINAL_ID, "uuid.7");
  await h.emit("session_shutdown"); assert.equal(h.timers.size, 0);
} else if (scenario === "idle-recovery") {
  for (const event of ["before_agent_start", "session_before_compact"]) {
    const h = await harness(); await h.emit("session_start"); h.idle(false); await h.emit(event);
    await h.tick(250); assert.deepEqual(h.statuses(), ["finished", "working"]);
    // Preparation failure / cancelled or failed manual compaction without agent_settled.
    h.idle(true); await h.tick(249); assert.equal(h.calls.length, 2);
    await h.tick(1); assert.deepEqual(h.statuses(), ["finished", "working", "finished"]);
    assert.equal(h.timers.size, 0); await h.tick(10000); assert.equal(h.calls.length, 3);
    await h.emit("session_shutdown");
  }
  const h = await harness(); h.idle(false); await h.emit("session_start"); assert.deepEqual(h.statuses(), ["working"]);
  h.invalidate(); await h.tick(250); assert.equal(h.timers.size, 0);
} else if (scenario === "retry") {
  for (const failure of [1, "error", "throw", "hang"]) {
    const h = await harness(); h.outcomes(failure, failure);
    const first = h.emit("session_start"); await flush();
    if (failure === "hang") await h.tick(2500);
    await first; assert.equal(h.calls.length, 1);
    await h.tick(250);
    if (failure === "hang") await h.tick(2500);
    assert.equal(h.calls.length, 2); assert.equal(h.warnings.length, 1);
    assert.deepEqual(Array.from(h.warnings[0]), ["xshell: Pi status reporting unavailable", "warning"]);
    assert.equal(h.timers.size, 0);
    if (failure === "hang") assert.ok(h.children.every(c => c.killed === "SIGKILL" && c.destroyed));
    await h.emit("session_start"); assert.equal(h.calls.length, 3); await h.emit("session_shutdown");
  }
} else if (scenario === "latest-and-uncertain-ack") {
  const h = await harness(); await h.emit("session_start");
  h.outcomes("hang"); h.idle(false);
  const working = h.emit("agent_start"); await flush();
  // The server may have applied Working, but its acknowledgement is lost.
  h.idle(true); const finished = h.emit("agent_settled"); await flush();
  assert.deepEqual(h.statuses(), ["finished", "working"]); // one child only
  await h.tick(2500); await working; await finished;
  assert.deepEqual(h.statuses(), ["finished", "working", "finished"]);
  await h.tick(1000); assert.equal(h.calls.length, 3); assert.equal(h.timers.size, 0);
  // A scheduled retry is superseded, not sent after a newer Finished.
  h.idle(false); h.outcomes(1); await h.emit("agent_start");
  await h.emit("agent_settled"); await h.tick(1000);
  assert.deepEqual(h.statuses(), ["finished", "working", "finished", "working", "finished"]);
  await h.emit("session_shutdown");
} else if (scenario === "generation-cleanup") {
  for (const reason of ["quit", "reload", "new", "resume", "fork"]) {
    const h = await harness(); h.idle(false); h.outcomes("hang");
    const pending = h.emit("session_start"); await flush();
    await h.emit("session_shutdown", { reason }); await pending;
    assert.equal(h.children[0].killed, "SIGKILL"); assert.equal(h.timers.size, 0);
    await h.tick(10000); assert.deepEqual(h.statuses(), ["working"]);
    h.idle(true); await h.emit("session_start"); assert.deepEqual(h.statuses(), ["working", "finished"]);
    h.outcomes(1); h.idle(false); await h.emit("agent_start");
    await h.emit("session_shutdown"); await h.tick(10000); assert.equal(h.calls.length, 3); assert.equal(h.timers.size, 0);
  }
} else if (scenario === "real-timeout") {
  const dir = await mkdtemp(join(tmpdir(), "x86-pi-helper-"));
  const helper = join(dir, "helper with ' spaces"); const pidFile = join(dir, "pids");
  try {
    await writeFile(helper, `#!${process.execPath}\nrequire('node:fs').appendFileSync(process.env.X86_PID_FILE, process.pid+'\\n');setInterval(()=>{},1000);\n`);
    await chmod(helper, 0o700);
    const h = await harness({ real: true, exe: helper, extraEnv: { X86_PID_FILE: pidFile } });
    const started = Date.now(); await h.emit("session_start");
    assert.ok(Date.now() - started >= 2400);
    await new Promise(resolve => setTimeout(resolve, 3000));
    await h.emit("session_shutdown");
    const pids = (await readFile(pidFile, "utf8")).trim().split("\n").map(Number);
    assert.equal(pids.length, 2); assert.equal(h.warnings.length, 1);
    for (const pid of pids) assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
    assert.ok(h.children.every(c => c.signalCode === "SIGKILL"));
  } finally { await rm(dir, { recursive: true, force: true }); }
} else throw new Error(`Unknown scenario ${scenario}`);
console.log(`PASS ${scenario}`);
