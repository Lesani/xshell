// A Ring member's Host id, as the backend names a Ring Host: "r_" + the 32 sign-key bytes in
// lowercase hex (hostlink `relay_host_id`). Member keys arrive as URL-safe base64 without
// padding (protocol `b64`). Null for anything that is not a 32-byte key.
export function ringHostId(key: string | null | undefined): string | null {
  if (!key) return null;
  try {
    const b64 = key.replace(/-/g, "+").replace(/_/g, "/");
    const bin = atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4));
    if (bin.length !== 32) return null;
    let hex = "";
    for (let i = 0; i < bin.length; i++) hex += bin.charCodeAt(i).toString(16).padStart(2, "0");
    return `r_${hex}`;
  } catch {
    return null;
  }
}
