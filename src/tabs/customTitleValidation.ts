import { fmt } from "../hosts/strings";

// Protocol Unicode White_Space deliberately excludes BOM.
export const normalizeCustomTitle = (text: string): string => text.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "");
export function customTitleError(text: string): string | null {
  if (/[\u0000-\u001f\u007f-\u009f\u2028\u2029]/u.test(text)) return fmt("tab.rename.controls");
  const normalized = normalizeCustomTitle(text);
  if (!normalized) return fmt("tab.rename.blank");
  return [...normalized].length > 256 ? fmt("tab.rename.tooLong") : null;
}

