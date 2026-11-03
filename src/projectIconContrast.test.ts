import { describe, expect, it } from "vitest";
import { projectIconContrast, isProjectIconText } from "./projectIconContrast";

describe("projectIconContrast", () => {
  it.each(["#ffffff", "#FFFF00", "#fff", "#ff0", "#808080", "#c96442"])("chooses black for the better WCAG ratio on %s", color => {
    expect(projectIconContrast(color)).toBe("#000000");
  });
  it.each(["#000000", "#000", "#123456", "#123", "#663399", "#444444"])("chooses white for the better WCAG ratio on %s", color => {
    expect(projectIconContrast(color)).toBe("#ffffff");
  });
  it("chooses the better WCAG ratio throughout an opaque RGB sample", () => {
    const levels = [0, 51, 102, 153, 204, 255];
    for (const r of levels) for (const g of levels) for (const b of levels) {
      const color = `#${[r, g, b].map(c => c.toString(16).padStart(2, "0")).join("")}`;
      const channels = [r, g, b].map(c => c / 255).map(c => c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4));
      const luminance = channels.reduce((sum, c, i) => sum + c * [0.2126, 0.7152, 0.0722][i], 0);
      const black = (luminance + 0.05) / 0.05, white = 1.05 / (luminance + 0.05);
      const picked = projectIconContrast(color) === "#000000" ? black : white;
      expect(picked, color).toBeGreaterThanOrEqual(Math.max(black, white));
      expect(picked, color).toBeGreaterThanOrEqual(4.5);
    }
  });
  it("uses linear-light contrast rather than a naive brightness threshold", () => {
    expect(projectIconContrast("#ff0000")).toBe("#000000");
    expect(projectIconContrast("#757575")).toBe("#ffffff");
    expect(projectIconContrast("#767676")).toBe("#000000");
  });
  it.each([undefined, "", "white", "rgb(255,255,255)", "#ffff", "#ffffff80", "#gggggg", "ffffff", " #ffffff"])("leaves the theme fallback alone for %s", color => {
    expect(projectIconContrast(color)).toBeUndefined();
  });
  it.each([undefined, "", "AB", "é", "漢字", "123", "x!"])("recognizes text/initials %s", icon => expect(isProjectIconText(icon)).toBe(true));
  it.each(["img:/fixture.png", "🚀", "⚡", "🇦🇹", "1️⃣", "#️⃣", "©️"])("does not override image or emoji artwork %s", icon => expect(isProjectIconText(icon)).toBe(false));
});
