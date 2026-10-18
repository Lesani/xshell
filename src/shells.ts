import type { ShellPreset } from "./types";

// Known shell presets. `command` is passed straight to the OS PTY spawn.
export const SHELL_PRESETS: ShellPreset[] = [
  { id: "powershell",  name: "Windows PowerShell", command: "powershell.exe", platforms: ["windows"] },
  { id: "pwsh",        name: "PowerShell 7",     command: "pwsh",           platforms: ["windows", "macos", "linux"] },
  { id: "cmd",         name: "Command Prompt",   command: "cmd.exe",        platforms: ["windows"] },
  { id: "gitbash",     name: "Git Bash",         command: "bash.exe",       platforms: ["windows"] },
  { id: "bash",        name: "Bash",             command: "bash",           platforms: ["macos", "linux"] },
  { id: "zsh",         name: "Zsh",              command: "zsh",            platforms: ["macos", "linux"] },
  { id: "fish",        name: "Fish",             command: "fish",           platforms: ["macos", "linux"] },
];

export type Platform = 'windows' | 'macos' | 'linux';

export function detectPlatform(): Platform {
  const ua = navigator.userAgent.toLowerCase();
  if (ua.includes("win")) return "windows";
  if (ua.includes("mac")) return "macos";
  return "linux";
}

// Shell presets for a platform. Remote Hosts pass their own OS (from `uname`), so a Windows
// Desktop never offers PowerShell/cmd for a Linux Host.
export function shellsForPlatform(platform: Platform): ShellPreset[] {
  return SHELL_PRESETS.filter(s => s.platforms.includes(platform));
}

export function defaultShellForPlatform(platform: Platform): string {
  if (platform === "windows") return "powershell";
  if (platform === "macos") return "zsh";
  return "bash";
}

// Missing Remote Host OS metadata keeps the existing Linux fallback; never infer it
// from the Desktop. Local Hosts may retain a configured preset valid for their OS.
export function defaultShellForHost(remote: boolean, hostOS: string | null | undefined, configuredShell: string, localPlatform?: Platform): ShellPreset {
  const platform = remote ? (hostOS ?? "linux") : (localPlatform ?? detectPlatform());
  const configured = getShellById(configuredShell);
  if (!remote && configured?.platforms.some(p => p === platform)) return configured;
  const fallback = platform === "windows" ? "windows" : platform === "macos" ? "macos" : "linux";
  return getShellById(defaultShellForPlatform(fallback))!;
}

export function getAvailableShells(): ShellPreset[] {
  return shellsForPlatform(detectPlatform());
}

export function getDefaultShellId(): string {
  return defaultShellForPlatform(detectPlatform());
}

export function getShellById(id: string): ShellPreset | undefined {
  return SHELL_PRESETS.find(s => s.id === id);
}
