import { describe, expect, it } from "vitest";
import { applyFenced, singleFlight } from "./startup";

describe("startup", () => {
  it("H: overlapping initialisation runs once; only the mounted run applies the delayed result", async () => {
    let calls = 0;
    let release!: (v: string) => void;
    const start = singleFlight(() => { calls++; return new Promise<string>(r => { release = r; }); });
    const applied: string[] = [];
    // StrictMode: mount, unmount (cleanup), mount again, all before the result.
    const cancel1 = applyFenced(start(), v => { applied.push(`first ${v}`); });
    cancel1();
    applyFenced(start(), v => { applied.push(`second ${v}`); });
    release("ok");
    await new Promise(r => setTimeout(r, 0));
    expect(calls).toBe(1);
    expect(applied).toEqual(["second ok"]);
    // A later caller gets the same result without running again.
    expect(await start()).toBe("ok");
    expect(calls).toBe(1);
  });

  it("7: alive() turns false once cancelled, so an apply stops after its next await", async () => {
    const steps: string[] = [];
    let go!: () => void;
    const gate = new Promise<void>(r => { go = r; });
    const cancel = applyFenced(Promise.resolve(1), async (_, alive) => {
      steps.push("first");
      await gate;
      if (!alive()) return;
      steps.push("second");
    });
    await new Promise(r => setTimeout(r, 0));
    cancel();
    go();
    await new Promise(r => setTimeout(r, 0));
    expect(steps).toEqual(["first"]);
  });

  it("a failed start applies nothing", async () => {
    const applied: unknown[] = [];
    applyFenced(Promise.reject(new Error("x")), v => { applied.push(v); });
    await new Promise(r => setTimeout(r, 0));
    expect(applied).toEqual([]);
  });
});
