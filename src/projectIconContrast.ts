// Configured Project colors are opaque hex. Unsupported colors retain the theme default.
export function projectIconContrast(color: string | undefined): "#000000" | "#ffffff" | undefined {
  if (!color || !/^#(?:[\da-f]{3}|[\da-f]{6})$/i.test(color)) return undefined;
  const hex = color.length === 4 ? color.slice(1).split("").map(c => c + c).join("") : color.slice(1);
  const linear = [0, 2, 4].map(i => {
    const channel = parseInt(hex.slice(i, i + 2), 16) / 255;
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  });
  const luminance = 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
  const blackContrast = (luminance + 0.05) / 0.05;
  const whiteContrast = 1.05 / (luminance + 0.05);
  return blackContrast >= whiteContrast ? "#000000" : "#ffffff";
}

// Preserve image/colored-emoji artwork; other custom strings and derived initials are text.
export const isProjectIconText = (icon: string | undefined): boolean =>
  !icon?.startsWith("img:") && !/[\p{Extended_Pictographic}\p{Regional_Indicator}\u20e3]/u.test(icon ?? "");
