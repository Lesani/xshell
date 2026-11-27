// @vitest-environment happy-dom
import { act } from "react";
import { createElement } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import type { Update } from "@tauri-apps/plugin-updater";

const order = vi.hoisted(() => [] as string[]);
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async (c: string) => { order.push(c); }) }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: vi.fn(async () => { order.push("relaunch"); }) }));
import { useInstaller, type Installer } from "./useInstaller";

it("local_quit_bypass is invoked before relaunch", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const update = { downloadAndInstall: async () => {} } as unknown as Update;
  let installer!: Installer;
  function H() { installer = useInstaller(update); return null; }
  const root = createRoot(document.createElement("div"));
  await act(async () => { root.render(createElement(H)); });
  await act(async () => { await installer.run(); });
  expect(order).toEqual(["local_quit_bypass", "relaunch"]);
  await act(async () => { root.unmount(); });
  vi.unstubAllGlobals();
});
