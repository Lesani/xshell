// RFC 9562 name-based UUID (version 5, SHA-1). Deterministic: the same name in the same
// namespace always gives the same UUID.

const hex = (b: Uint8Array) => Array.from(b, x => x.toString(16).padStart(2, "0")).join("");

function parse(uuid: string): Uint8Array {
  const h = uuid.replace(/-/g, "");
  if (!/^[0-9a-f]{32}$/i.test(h)) throw new Error(`not a UUID: ${uuid}`);
  const out = new Uint8Array(16);
  for (let i = 0; i < 16; i++) out[i] = parseInt(h.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export async function uuidV5(name: string, namespace: string): Promise<string> {
  const ns = parse(namespace);
  const n = new TextEncoder().encode(name);
  const buf = new Uint8Array(ns.length + n.length);
  buf.set(ns);
  buf.set(n, ns.length);
  const b = new Uint8Array(await crypto.subtle.digest("SHA-1", buf)).slice(0, 16);
  b[6] = (b[6] & 0x0f) | 0x50;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = hex(b);
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

// Namespace of the Terminal UUIDs that in-process Local Tabs get when they move into the
// Daemon: the UUID of a saved Tab is `uuidV5(tab.id, MIGRATION_NS)`.
export const MIGRATION_NS = "fc4bc7cb-14b0-41e5-a00a-d1fab3d840db";
