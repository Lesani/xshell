import { createElement } from "react";
import { expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => ({ configs: [], status: {}, live: {} }) }));
import { ProjectPicker } from "./ProjectPicker";

it("parent-merged ProjectPicker input never fabricates Claude discovery before raw lists load", () => {
  const html = renderToStaticMarkup(createElement(ProjectPicker, { allProjects: [{ name: "Pi only", path: "/pi-only", encoded_name: "-pi-only", session_count: 7, last_active: "2026-10-18" }], savedPaths: [], onToggle: () => {}, onBrowse: () => {}, onClose: () => {} }));
  expect(html).not.toContain("picker-agent");
  expect(html).not.toContain("/pi-only");
});
