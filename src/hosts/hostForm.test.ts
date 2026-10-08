import { describe, expect, it } from "vitest";
import { hintText, toHostConfig, validateHostForm } from "./hostForm";
import { HOST_ID_RE } from "./projectKey";

const ok = { name: "Dev", sshTarget: "user@host", daemonCommand: "" };

describe("host form", () => {
  it("accepts an alias or user@host", () => {
    expect(validateHostForm(ok)).toEqual({});
    expect(validateHostForm({ ...ok, sshTarget: "dev" })).toEqual({});
  });

  it("rejects what the Rust validation rejects", () => {
    expect(validateHostForm({ ...ok, name: "  " }).name).toBe("Enter a name for this host.");
    expect(validateHostForm({ ...ok, sshTarget: "" }).sshTarget).toBe("Enter an SSH alias or user@host.");
    for (const bad of ["-oProxyCommand=x", "a b", "a\tb", "x".repeat(256), "a\u0007b"]) {
      expect(validateHostForm({ ...ok, sshTarget: bad }).sshTarget).toMatch(/^Enter an SSH alias or user@host: at most 255/);
    }
    expect(validateHostForm({ ...ok, daemonCommand: "x\ny" }).daemonCommand).toBe("Enter a non-empty command on one line, or leave it blank.");
    expect(validateHostForm({ ...ok, daemonCommand: "   " }).daemonCommand).toBeDefined();
    expect(validateHostForm({ ...ok, daemonCommand: "~/bin/xshelld" })).toEqual({});
  });

  it("builds a config with a fresh id and no blank optionals", () => {
    const c = toHostConfig({ name: " Dev ", sshTarget: " dev ", daemonCommand: " " });
    expect(c).toEqual({ id: c.id, name: "Dev", sshTarget: "dev" });
    expect(HOST_ID_RE.test(c.id)).toBe(true);
    expect(toHostConfig({ ...ok, color: "#3498DB", daemonCommand: "~/xd" }, "h_ab12cd34")).toEqual({ id: "h_ab12cd34", name: "Dev", sshTarget: "user@host", color: "#3498DB", daemonCommand: "~/xd" });
  });

  it("hints name the configured target", () => {
    expect(hintText("host-key", "dev")).toBe("Run `ssh dev` once in a terminal to accept the key.");
    expect(hintText("permission-denied", "u@h")).toBe("Set up SSH key access; verify `ssh -o BatchMode=yes u@h` works.");
    expect(hintText(null, "dev")).toBeNull();
  });
});
