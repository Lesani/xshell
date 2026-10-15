import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/plugin-store", () => ({ load: vi.fn() }));

import { statusLabel } from "./useHosts";
import type { HostStatus } from "./types";

describe("statusLabel", () => {
  it("names every status, build differs included (#49)", () => {
    const label = (status: HostStatus["status"]) => statusLabel({ status } as HostStatus);
    expect(label("build-differs")).toBe("Different build");
    expect(label("upgrade-pending")).toBe("Upgrade pending");
    expect(label("connected")).toBe("Connected");
    expect(statusLabel(undefined)).toBe("Reconnecting");
  });
});
