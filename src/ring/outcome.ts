// "Added …" / "Removed …" lines describe the last action only: they go away by themselves.
let ms = 8000;
export const outcomeMs = () => ms;
/** Tests shorten the lifetime. */
export function setOutcomeMs(v: number) { ms = v; }
