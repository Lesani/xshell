import { createElement } from "react";
import { expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { AGENTS, AGENT_IDS, AgentIcon } from "./agents";
import { PiIcon } from "./components/PiIcon";

it("Pi registry probes pi and never offers bypass", () => {
  expect(AGENT_IDS).toContain("pi");
  expect(AGENTS.pi).toMatchObject({ label: "Pi", binary: "pi", bypassFlag: false });
  expect(renderToStaticMarkup(createElement(AgentIcon, { agent: "pi" }))).toBe(renderToStaticMarkup(createElement(PiIcon)));
  expect(renderToStaticMarkup(createElement(AgentIcon, { agent: "pi" }))).not.toBe(renderToStaticMarkup(createElement(AgentIcon, { agent: "claude" })));
});
