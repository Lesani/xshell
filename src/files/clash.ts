import type { StringKey } from "../hosts/strings";
import type { ClashChoice, UploadEntriesResult } from "./transfer";

// Name clashes of a drop-in upload (Lesani/xshell#46, D4): the user picks Replace, Keep both
// or Skip; "Apply to all" answers the rest of the same batch without asking again.

export interface ClashAnswer {
  choice: ClashChoice;
  applyAll: boolean;
}

// A resolver for one batch (`uploadEntries`' `resolveClash`). `ask` shows the dialog for a
// path. Each batch gets its own resolver, so "Apply to all" never outlives the batch.
export function clashResolver(ask: (path: string) => Promise<ClashAnswer>): (path: string) => Promise<ClashChoice> {
  let remembered: ClashChoice | null = null;
  return async (path) => {
    if (remembered) return remembered;
    const a = await ask(path);
    if (a.applyAll) remembered = a.choice;
    return a.choice;
  };
}

// The summary notice of a finished batch.
export function uploadSummary(r: UploadEntriesResult, folder: string): { key: StringKey; params: Record<string, string | number>; tone: "info" | "warn" } {
  const n = r.uploaded.length;
  if (r.skipped.length === 0 && r.failed.length === 0) return { key: "explorer.drop.done", params: { n, folder }, tone: "info" };
  return { key: "explorer.drop.partial", params: { n, skipped: r.skipped.length, failed: r.failed.length }, tone: "warn" };
}
