// Display formatting and input parsing. Pure, so it is unit-tested.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/** 1536 → "1.5 KiB". */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "–";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

export function formatSpeed(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

/** 3725 → "1h 2m", 75 → "1m 15s", 9 → "9s". `null` when unknown. */
export function formatEta(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds) || seconds < 0) return "–";
  const total = Math.round(seconds);
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes}m ${total % 60}s`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ${minutes % 60}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

/** Seconds left, or `null` when the speed or the size is unknown. */
export function etaSeconds(
  size: number | null,
  downloaded: number,
  speed: number,
): number | null {
  if (size === null || speed <= 0) return null;
  return Math.max(0, size - downloaded) / speed;
}

/** 0–100, or `null` when the size is unknown. */
export function percent(size: number | null, downloaded: number): number | null {
  if (size === null) return null;
  if (size === 0) return 100;
  return Math.min(100, Math.floor((downloaded / size) * 100));
}

const RATE = /^\s*(\d+(?:\.\d+)?)\s*([kmg]?)(?:i?b)?(?:\/s)?\s*$/i;

/**
 * Parses a speed limit like "500K", "5M", "1.5 MB/s" into bytes per
 * second (binary units). Empty, "0", "none", "off" and "unlimited" mean
 * no limit (`null`). Anything else unparseable is `undefined`.
 */
export function parseRate(text: string): number | null | undefined {
  const trimmed = text.trim().toLowerCase();
  if (["", "0", "none", "off", "unlimited"].includes(trimmed)) return null;
  const match = RATE.exec(trimmed);
  if (!match) return undefined;
  const value = Number(match[1]);
  const multiplier = { "": 1, k: 1024, m: 1024 ** 2, g: 1024 ** 3 }[
    match[2] as "" | "k" | "m" | "g"
  ];
  const bytes = Math.round(value * multiplier);
  return bytes > 0 ? bytes : null;
}

/** The inverse of `parseRate`, for filling a text field: 5242880 → "5M". */
export function rateText(bytesPerSecond: number | null): string {
  if (bytesPerSecond === null) return "";
  for (const [suffix, size] of [
    ["G", 1024 ** 3],
    ["M", 1024 ** 2],
    ["K", 1024],
  ] as const) {
    if (bytesPerSecond >= size) {
      const value = bytesPerSecond / size;
      return `${Number.isInteger(value) ? value : value.toFixed(1)}${suffix}`;
    }
  }
  return String(bytesPerSecond);
}

/** Splits pasted text into links: one per line or whitespace-separated. */
export function splitLinks(text: string): string[] {
  const seen = new Set<string>();
  const links: string[] = [];
  for (const part of text.split(/\s+/)) {
    const link = part.trim();
    if (link && !seen.has(link)) {
      seen.add(link);
      links.push(link);
    }
  }
  return links;
}
