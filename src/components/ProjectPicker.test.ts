import { createElement } from "react";
import { describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), Channel: class {} }));
vi.mock("../hosts/useHosts", async original => ({ ...await original<typeof import("../hosts/useHosts")>(), useHostsSnapshot: () => ({ configs: [], available: [], status: {}, live: {} }) }));
import { ProjectPicker } from "./ProjectPicker";

it("parent-merged ProjectPicker input never fabricates Claude discovery before raw lists load", () => {
  const html = renderToStaticMarkup(createElement(ProjectPicker, { allProjects: [{ name: "Pi only", path: "/pi-only", encoded_name: "-pi-only", session_count: 7, last_active: "2026-10-18" }], savedPaths: [], onToggle: () => {}, onBrowse: () => {}, onClose: () => {} }));
  expect(html).not.toContain("picker-agent");
  expect(html).not.toContain("/pi-only");
});

describe("add-path example", () => {
  it("follows the Host's OS", async () => {
    const { addPathPlaceholder } = await import("./ProjectPicker");
    expect(addPathPlaceholder("windows")).toBe("C:\\Users\\user\\project");
    expect(addPathPlaceholder("linux")).toBe("/home/user/project");
    expect(addPathPlaceholder(null)).toBe("/home/user/project");
  });
});
