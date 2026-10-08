// Amendment 23: components with path-only effects key them on the ProjectKey and ignore
// responses from a superseded key. `latestGate()` hands out a token per request; only the
// newest token is current. Two Hosts with the same path produce different keys, so a slow
// response for the previous Host is dropped.
export interface RequestGate {
  begin(key: string): number;
  isCurrent(token: number): boolean;
  currentKey(): string | null;
}

export function latestGate(): RequestGate {
  let seq = 0;
  let key: string | null = null;
  return {
    begin(k: string) { key = k; return ++seq; },
    isCurrent(token: number) { return token === seq; },
    currentKey() { return key; },
  };
}
