// The Desktop's startup runs once per window (H, M7): React StrictMode runs the startup
// effect twice, and only one run may read, migrate and write. Its result is applied by the
// effect run that is still mounted when it arrives, and by no other.

export function singleFlight<T>(fn: () => Promise<T>): () => Promise<T> {
  let p: Promise<T> | null = null;
  return () => (p ??= fn());
}

// Applies `p`'s result unless the returned cancel ran first (the effect's cleanup). `apply`
// gets `alive()` to check again after each of its own awaits.
export function applyFenced<T>(p: Promise<T>, apply: (v: T, alive: () => boolean) => void | Promise<void>): () => void {
  let live = true;
  const alive = () => live;
  p.then(v => { if (live) return apply(v, alive); }).catch(() => {});
  return () => { live = false; };
}
