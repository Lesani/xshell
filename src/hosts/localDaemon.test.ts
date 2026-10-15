import { describe, expect, it } from "vitest";
import { StrictMode, createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { confirmAgainCount, controls, errorCode, initialState, trackMounted, localDaemonLine, otherBuildNote, persistentVisible, reduce, replaceConfirm, startsSwitch, startsUpgrade, switchConfirm, switchErrorText, upgradeConfirm, type Action, type State } from "./localDaemon";
import type { BuildId } from "../buildId";
import type { LocalPersistentInfo } from "./localHost";
import { LocalDaemonSection, LocalDaemonView } from "../components/LocalDaemonSettings";
import { S } from "./strings";
import type { HostStatus, TerminalInfo } from "./types";

const on: LocalPersistentInfo = { supported: true, enabled: true, running: "persistent", log: "/home/u/.xshell/log/xshelld.log" };
const off: LocalPersistentInfo = { ...on, enabled: false, running: "gui-bound" };

const status = (patch: Partial<HostStatus> = {}): HostStatus => ({
  host: "local", status: "connected", phase: null, lastError: null, errorHint: null, nextRetryAt: null,
  daemonVersion: "1.5.0", desktopVersion: "1.5.0", protocol: 1, daemonCapabilities: [], configGeneration: 0,
  incompatibleReason: null, os: null, arch: null, ...patch,
} as HostStatus);

const ours: BuildId = { commit: "85ee920a1b2c", dirty: null, time: 1760000000 };
const other: BuildId = { commit: "000000000000", dirty: null, time: 1760000000 };

const term = (): TerminalInfo => ({ terminal: crypto.randomUUID() } as unknown as TerminalInfo);

const run = (actions: Action[], s: State = initialState) => actions.reduce(reduce, s);

describe("localDaemon", () => {
  it("is shown only where the backend supports it", () => {
    expect(persistentVisible(null)).toBe(false);
    expect(persistentVisible({ mode: "in-process", reason: "r", persistent: on })).toBe(false);
    expect(persistentVisible({ mode: "daemon", reason: null })).toBe(false);
    expect(persistentVisible({ mode: "daemon", reason: null, persistent: { ...on, supported: false } })).toBe(false);
    expect(persistentVisible({ mode: "daemon", reason: null, persistent: on })).toBe(true);
  });

  it("confirms a switch only when terminals restart, with their count", () => {
    expect(switchConfirm(true, 0)).toBeNull();
    expect(switchConfirm(false, 0)).toBeNull();
    const c = switchConfirm(true, 3)!;
    expect(c.title).toBe(S["local.persistent.onTitle"]);
    expect(c.body).toContain("all 3 terminals");
    expect(c.confirm).toBe("Turn on");
    const d = switchConfirm(false, 2)!;
    expect(d.title).toBe(S["local.persistent.offTitle"]);
    expect(d.body).toContain("all 2 terminals");
    expect(d.body).toContain("including those opened from other devices");
    expect(d.confirm).toBe("Turn off");
    expect(upgradeConfirm(4).body).toBe("Upgrading xshell on this computer restarts all 4 terminals, including those used by others. Agent sessions resume; shells start fresh in the same directory.");
  });

  it("maps error codes to text", () => {
    expect(switchErrorText("other-app", null)).toBe(S["local.persistent.err.otherApp"]);
    expect(switchErrorText("timeout", "/l/xshelld.log")).toBe("Local terminals did not become available in time. Check /l/xshelld.log for details.");
    expect(switchErrorText("failed:disk full", null)).toBe("Couldn’t change terminal settings: disk full");
    expect(switchErrorText("weird", null)).toBe("Couldn’t change terminal settings: weird");
    expect(errorCode("timeout")).toBe("timeout");
    expect(errorCode({ code: "offline", message: "local is offline" })).toBe("local is offline");
  });

  it("never says daemon to the user", () => {
    for (const [k, v] of Object.entries(S)) if (k.startsWith("local.")) expect(v.toLowerCase()).not.toContain("daemon");
  });

  it("the setup line names the version and mode once connected", () => {
    expect(localDaemonLine(status(), "persistent")).toBe("Terminal setup 1.5.0 · keeps running");
    expect(localDaemonLine(status(), "gui-bound")).toBe("Terminal setup 1.5.0 · ends with xshell");
    expect(localDaemonLine(status({ status: "upgrade-pending", daemonVersion: "1.4.0" }), "persistent")).toBe("Terminal setup 1.4.0 · keeps running");
    expect(localDaemonLine(status(), null)).toBeNull();
    expect(localDaemonLine(undefined, "persistent")).toBeNull();
    // With the build, once the backend reports it (#49).
    expect(localDaemonLine(status({ daemonBuild: ours }), "persistent")).toBe("Terminal setup 1.5.0 (85ee920a1b2c) · keeps running");
    expect(localDaemonLine(status({ daemonBuild: null }), "gui-bound")).toBe("Terminal setup 1.5.0 (unknown build) · ends with xshell");
  });

  it("replace (#49): offered for a Persistent Daemon of another build, through the upgrade phases", () => {
    const differs = status({ status: "build-differs", daemonBuild: null, desktopBuild: ours });
    expect(controls(initialState, on, differs, []).showReplace).toBe(true);
    expect(controls(initialState, on, differs, []).showUpgrade).toBe(false);
    expect(controls(initialState, off, differs, []).showReplace).toBe(false);
    // A GUI-bound Daemon of another build: no action, only the note.
    expect(controls(initialState, { ...on, running: "gui-bound" }, differs, []).showReplace).toBe(false);
    expect(controls(initialState, on, status(), []).showReplace).toBe(false);
    const asked = run([{ type: "replace", terminals: 2, build: "85ee920a1b2c" }]);
    expect(asked.phase).toEqual({ kind: "confirm-upgrade", confirm: replaceConfirm(2, "85ee920a1b2c"), replace: true });
    expect(reduce(asked, { type: "cancel" }).phase.kind).toBe("idle");
    const up = reduce(asked, { type: "confirm" });
    expect(up.phase).toEqual({ kind: "upgrading", replace: true });
    expect(startsUpgrade(asked, up)).toBe(true);
    const c = controls(up, on, differs, []);
    expect(c.replaceBusy).toBe(true);
    expect(c.upgradeBusy).toBe(false);
    expect(c.showUpgrade).toBe(false);
    expect(c.disabled).toBe(true);
    expect(reduce(up, { type: "upgraded" }).phase.kind).toBe("idle");
    expect(reduce(up, { type: "failed", error: "e" })).toEqual({ phase: { kind: "idle" }, error: "e" });
    // The Host's replacement in progress keeps the button busy.
    expect(controls(initialState, on, { ...differs, phase: "upgrading" }, []).replaceBusy).toBe(true);
    // Busy elsewhere: ignored.
    expect(reduce(up, { type: "replace", terminals: 0, build: "x" })).toBe(up);
  });

  it("replace confirmation names this computer and the build", () => {
    const c = replaceConfirm(3, "85ee920a1b2c");
    expect(c.title).toBe("Replace host setup?");
    expect(c.body).toBe("Replacing xshell on this computer with build 85ee920a1b2c restarts all 3 terminals there, including those used by other Desktops. Agent sessions resume; shells start fresh in the same directory. Desktops on other builds will offer to switch back.");
    expect(c.confirm).toBe("Replace");
  });

  it("another app's GUI-bound Daemon of another build: the quit note only", () => {
    const st = (p: Partial<HostStatus>) => status({ desktopBuild: ours, ...p });
    expect(otherBuildNote(st({ daemonBuild: other }), "gui-bound")).toBe("Another xshell app using 000000000000 runs this computer’s terminals. Quit that xshell app to use this Desktop’s build.");
    expect(otherBuildNote(st({ daemonBuild: null }), "gui-bound")).toBe("Another xshell app using unknown build runs this computer’s terminals. Quit that xshell app to use this Desktop’s build.");
    expect(otherBuildNote(st({ daemonBuild: { ...ours, time: 1 } }), "gui-bound")).toBeNull();
    expect(otherBuildNote(st({ daemonBuild: other }), "persistent")).toBeNull();
    expect(otherBuildNote(st({ daemonBuild: other, status: "offline" }), "gui-bound")).toBeNull();
    // Unknown own build, or a backend that reports no builds: nothing to compare.
    expect(otherBuildNote(st({ daemonBuild: other, desktopBuild: null }), "gui-bound")).toBeNull();
    expect(otherBuildNote(status(), "gui-bound")).toBeNull();
  });

  it("toggle with terminals asks first; cancel changes nothing", () => {
    const asked = run([{ type: "toggle", target: true, terminals: 2 }]);
    expect(asked.phase.kind).toBe("confirm-switch");
    expect(startsSwitch(initialState, asked)).toBeNull();
    const cancelled = reduce(asked, { type: "cancel" });
    expect(cancelled.phase.kind).toBe("idle");
    expect(controls(cancelled, off, status(), [term(), term()]).checked).toBe(false);
    const confirmed = reduce(asked, { type: "confirm" });
    expect(startsSwitch(asked, confirmed)).toEqual({ target: true, confirmed: 2 });
  });

  it("toggle without terminals switches at once", () => {
    const s = run([{ type: "toggle", target: false, terminals: 0 }]);
    expect(s.phase).toEqual({ kind: "switching", target: false, terminals: 0 });
    expect(startsSwitch(initialState, s)).toEqual({ target: false, confirmed: 0 });
  });

  it("busy: the toggle keeps its value, is disabled, and blocks upgrade", () => {
    const s = run([{ type: "toggle", target: false, terminals: 0 }]);
    const c = controls(s, on, status({ status: "upgrade-pending" }), []);
    expect(c.checked).toBe(true);
    expect(c.disabled).toBe(true);
    expect(c.hint).toBe("Switching…");
    expect(c.upgradeDisabled).toBe(true);
    // A second toggle or an upgrade request while busy is ignored.
    expect(reduce(s, { type: "toggle", target: true, terminals: 0 })).toBe(s);
    expect(reduce(s, { type: "upgrade", terminals: 0 })).toBe(s);
  });

  it("failure returns to idle with the error, keeping the stored value", () => {
    const s = run([{ type: "toggle", target: true, terminals: 0 }, { type: "failed", error: "boom" }]);
    expect(s).toEqual({ phase: { kind: "idle" }, error: "boom" });
    expect(controls(s, off, status(), []).checked).toBe(false);
    // The next attempt clears it.
    expect(reduce(s, { type: "toggle", target: true, terminals: 1 }).error).toBeNull();
    expect(run([{ type: "switched" }], run([{ type: "toggle", target: true, terminals: 0 }])).phase.kind).toBe("idle");
  });

  it("an unknown terminal list blocks switching rather than counting as none", () => {
    const c = controls(initialState, off, status({ status: "reconnecting" }), null);
    expect(c.disabled).toBe(true);
    expect(c.hint).toBe("Switching…");
    expect(c.checked).toBe(false);
    expect(controls(initialState, off, status(), []).disabled).toBe(false);
  });

  it("upgrade: offered when pending with the setting on, and blocks switching", () => {
    expect(controls(initialState, on, status({ status: "upgrade-pending" }), []).showUpgrade).toBe(true);
    expect(controls(initialState, on, status({ status: "incompatible", incompatibleReason: "daemon-older" }), []).showUpgrade).toBe(true);
    expect(controls(initialState, on, status({ status: "incompatible", incompatibleReason: "daemon-newer" }), []).showUpgrade).toBe(false);
    expect(controls(initialState, off, status({ status: "upgrade-pending" }), []).showUpgrade).toBe(false);
    expect(controls(initialState, on, status(), []).showUpgrade).toBe(false);
    const asked = run([{ type: "upgrade", terminals: 3 }]);
    expect(asked.phase.kind).toBe("confirm-upgrade");
    expect(reduce(asked, { type: "cancel" }).phase.kind).toBe("idle");
    const up = reduce(asked, { type: "confirm" });
    expect(startsUpgrade(asked, up)).toBe(true);
    const c = controls(up, on, status({ status: "upgrade-pending" }), []);
    expect(c.disabled).toBe(true);
    expect(c.upgradeBusy).toBe(true);
    expect(reduce(up, { type: "upgraded" }).phase.kind).toBe("idle");
    // The Host's own upgrade phase blocks the toggle too.
    expect(controls(initialState, on, status({ phase: "upgrading" }), []).disabled).toBe(true);
  });
});

describe("localDaemon: the live list and confirm-again", () => {
  it("switching needs a usable connection, not a list kept from an earlier one", () => {
    const list = [term()];
    for (const st of ["reconnecting", "offline", "incompatible"] as const) {
      const c = controls(initialState, off, status({ status: st }), list);
      expect(c.disabled).toBe(true);
      expect(c.hint).toBe("Switching…");
    }
    expect(controls(initialState, off, undefined, list).disabled).toBe(true);
    expect(controls(initialState, off, status(), list).disabled).toBe(false);
  });

  it("parses confirm-again codes", () => {
    expect(confirmAgainCount("confirm-again:3")).toBe(3);
    expect(confirmAgainCount("confirm-again")).toBe(1);
    expect(confirmAgainCount("timeout")).toBeNull();
    expect(confirmAgainCount("failed:confirm-again:3")).toBeNull();
  });

  it("confirm-again re-asks with the new count, then sends that count", () => {
    const s = run([{ type: "toggle", target: true, terminals: 0 }]);
    expect(startsSwitch(initialState, s)).toEqual({ target: true, confirmed: 0 });
    const again = reduce(s, { type: "confirm-again", terminals: 2 });
    expect(again.phase.kind).toBe("confirm-switch");
    expect(again.phase.kind === "confirm-switch" && again.phase.confirm.body).toContain("all 2 terminals");
    const go = reduce(again, { type: "confirm" });
    expect(startsSwitch(again, go)).toEqual({ target: true, confirmed: 2 });
    // Cancelling the re-ask changes nothing.
    expect(reduce(again, { type: "cancel" })).toEqual({ phase: { kind: "idle" }, error: null });
    // Never re-asks for fewer than were confirmed + 1, so it cannot loop on a stale count.
    const stale = reduce(run([{ type: "toggle", target: false, terminals: 2 }, { type: "confirm" }]), { type: "confirm-again", terminals: 0 });
    expect(stale.phase.kind === "confirm-switch" && stale.phase.terminals).toBe(3);
    // Only while switching.
    expect(reduce(initialState, { type: "confirm-again", terminals: 2 })).toBe(initialState);
  });

  // No DOM library is installed, so React's StrictMode double run (setup, cleanup, setup) is
  // replayed on the effect function itself.
  it("the mounted flag survives StrictMode's effect replay", () => {
    const ref = { current: false };
    const cleanup = trackMounted(ref);
    expect(ref.current).toBe(true);
    cleanup();
    expect(ref.current).toBe(false);
    const cleanup2 = trackMounted(ref);
    expect(ref.current).toBe(true);
    cleanup2();
    expect(ref.current).toBe(false);
  });
});

describe("LocalDaemonView", () => {
  it("renders under StrictMode", () => {
    const html = renderToStaticMarkup(createElement(StrictMode, null, createElement(LocalDaemonView, { persistent: on, status: status(), live: [], state: initialState, dispatch: () => {} })));
    expect(html).toContain("Keep terminals running after quit");
  });

  const render = (state: State, persistent = off, st: HostStatus | undefined = status(), live: TerminalInfo[] | null = [term()]) =>
    renderToStaticMarkup(createElement(LocalDaemonView, { persistent, status: st, live, state, dispatch: () => {} }));

  it("idle: the copy, an enabled toggle with the stored value, and the setup line", () => {
    const html = render(initialState, on);
    expect(html).toContain("This computer");
    expect(html).toContain("Keep terminals running after quit");
    expect(html).toMatch(/<input type="checkbox"[^>]*checked=""/);
    expect(html).not.toMatch(/<input[^>]*disabled=""/);
    expect(html).toContain("Terminal setup 1.5.0 · keeps running");
    expect(html).not.toContain("Upgrade now");
  });

  it("busy: disabled but still checked, with the switching hint", () => {
    const html = render(run([{ type: "toggle", target: false, terminals: 0 }]), on);
    const input = html.match(/<input[^>]*>/)![0];
    expect(input).toContain('checked=""');
    expect(input).toContain('disabled=""');
    expect(html).toContain("Switching…");
  });

  it("confirm: the dialog with the count", () => {
    const html = render(run([{ type: "toggle", target: true, terminals: 1 }]));
    expect(html).toContain("Keep terminals running after quit?");
    expect(html).toContain("This restarts all 1 terminals on this computer.");
    expect(html).toContain("Turn on");
  });

  it("failure: the error text", () => {
    const html = render(run([{ type: "toggle", target: true, terminals: 0 }, { type: "failed", error: switchErrorText("other-app", null) }]));
    expect(html).toContain("Another xshell window controls these terminals.");
    expect(html).not.toContain("Switching…");
  });

  it("build differs: the chip, the build in the line, and Replace (#49)", () => {
    const html = render(initialState, on, status({ status: "build-differs", daemonBuild: null, desktopBuild: ours }));
    expect(html).toContain("Different build");
    expect(html).toContain("Terminal setup 1.5.0 (unknown build) · keeps running");
    expect(html).toContain("Replace with Desktop’s build");
    expect(html).not.toContain("Upgrade now");
    const busy = render({ phase: { kind: "upgrading", replace: true }, error: null }, on, status({ status: "build-differs", daemonBuild: null, desktopBuild: ours }));
    expect(busy).toContain("Replacing…");
  });

  it("another app on another build: the note, no action", () => {
    const html = render(initialState, { ...on, enabled: false, running: "gui-bound" }, status({ daemonBuild: other, desktopBuild: ours }));
    expect(html).toContain("Another xshell app using 000000000000 runs this computer’s terminals.");
    expect(html).not.toContain("Replace with");
  });

  it("C4: the command succeeded, then the status names why the replacement did not take", () => {
    const c4 = "xshelld 1.5.0 (unknown build) is still running after replacement; expected 1.5.0 (85ee920a1b2c). Try Replace again. If this repeats, quit other xshell apps using this host.";
    const st = status({ status: "build-differs", daemonBuild: null, desktopBuild: ours, lastError: c4 });
    const html = render(run([{ type: "replace", terminals: 0, build: "85ee920a1b2c" }, { type: "confirm" }, { type: "upgraded" }]), on, st);
    expect(html).toContain("Last error");
    expect(html).toContain("still running after replacement");
    expect(html).toContain("Replace with Desktop’s build");
    // Shown once when the section's own error already says it.
    const both = render({ phase: { kind: "idle" }, error: `Couldn’t change terminal settings: ${c4}` }, on, st);
    expect(both.split("still running after replacement").length - 1).toBe(1);
    // Not for a plainly connected Host.
    expect(render(initialState, on, status({ lastError: "old" }))).not.toContain("Last error");
  });

  it("without persistence (Windows): build and the other-app note, no toggle or Replace", () => {
    const section = (info: Parameters<typeof LocalDaemonSection>[0]["info"], st: HostStatus) =>
      renderToStaticMarkup(createElement(LocalDaemonSection, { info, status: st, live: [term()], state: initialState, dispatch: () => {} }));
    const unsupported = { mode: "daemon" as const, reason: null, persistent: { supported: false, enabled: false, running: null, log: null } };
    const html = section(unsupported, status({ status: "connected", daemonBuild: other, desktopBuild: ours }));
    expect(html).toContain("This computer");
    expect(html).toContain("Terminal setup 1.5.0 (000000000000) · ends with xshell");
    expect(html).toContain("Another xshell app using 000000000000 runs this computer’s terminals.");
    expect(html).not.toContain("Keep terminals running after quit");
    expect(html).not.toContain("<input");
    expect(html).not.toContain("Replace with");
    expect(html).not.toContain("Choose what happens");
    // A backend without the persistence info at all, and the same build: just the line.
    const same = section({ mode: "daemon", reason: null }, status({ daemonBuild: ours, desktopBuild: ours }));
    expect(same).toContain("Terminal setup 1.5.0 (85ee920a1b2c) · ends with xshell");
    expect(same).not.toContain("Another xshell app");
    // In-process local Tabs: nothing.
    expect(section({ mode: "in-process", reason: "r" }, status())).toBe("");
    expect(section(null, status())).toBe("");
    // Supported: the toggle is there.
    expect(section({ mode: "daemon", reason: null, persistent: on }, status())).toContain("Keep terminals running after quit");
  });

  it("upgrade pending: the chip and Upgrade now", () => {
    const html = render(initialState, on, status({ status: "upgrade-pending", daemonVersion: "1.4.0" }));
    expect(html).toContain("Upgrade pending");
    expect(html).toContain("Upgrade now");
  });
});
