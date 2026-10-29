import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Action } from "../hosts/localDaemon";
import type { HostStatus } from "../hosts/types";

const test = vi.hoisted(() => ({ invoke: vi.fn().mockResolvedValue(undefined), status: {} as HostStatus }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: test.invoke }));
vi.mock("../hosts/useHosts", () => ({ useHostsSnapshot: () => ({ status: { local: test.status }, live: { local: [] } }), statusLabel: () => "" }));
// Exercise the actual settings dispatch handler without a DOM renderer. Its refs hold the
// reducer/confirmation state; rendering and lifecycle effects are not part of this test.
vi.mock("react", async importOriginal => ({
  ...await importOriginal<typeof import("react")>(),
  useState: (value: unknown) => [typeof value === "function" ? value() : value, () => {}],
  useRef: (value: unknown) => ({ current: value }),
  useEffect: () => {},
  useCallback: (callback: unknown) => callback,
}));
import { LocalDaemonSettings } from "./LocalDaemonSettings";

beforeEach(() => {
  test.invoke.mockClear();
  test.status = { host: "local", status: "upgrade-pending", configGeneration: 7, linkGeneration: 12, daemonVersion: "1.4.0", daemonBuild: { commit: "000000000000", dirty: null, time: 1 } } as HostStatus;
});

describe("LocalDaemonSettings confirmed update invocation", () => {
  for (const action of [{ type: "upgrade", terminals: 2 }, { type: "replace", terminals: 2, build: "85ee920a1b2c" }] satisfies Action[]) {
    it(`supplies the captured target for ${action.type}, not the status at confirmation time`, () => {
      const settings = LocalDaemonSettings();
      const dispatch = settings.props.dispatch as (action: Action) => void;
      dispatch(action);
      const target = { configGeneration: 7, linkGeneration: 12, daemonVersion: "1.4.0", daemonBuild: { commit: "000000000000", dirty: null, time: 1 } };
      test.status.configGeneration = 8;
      test.status.linkGeneration = 13;
      test.status.daemonVersion = "1.5.0";
      test.status.daemonBuild!.commit = "111111111111";
      dispatch({ type: "confirm" });
      expect(test.invoke).toHaveBeenCalledExactlyOnceWith("host_upgrade", { host: "local", target });
    });
  }
});
