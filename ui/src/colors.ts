// A colour for every link, stable across runs: the user's choice, else a
// palette entry picked from the interface name.

const PALETTE = [
  "#3b82f6",
  "#f0662d",
  "#1fa971",
  "#5f7d95",
  "#c98500",
  "#0b97b8",
  "#e0457f",
  "#6a9a1f",
];

function hash(text: string): number {
  let h = 5381;
  for (let i = 0; i < text.length; i += 1) {
    h = ((h << 5) + h + text.charCodeAt(i)) >>> 0;
  }
  return h;
}

export function linkColor(name: string, chosen: string | null): string {
  if (chosen && /^#[0-9a-f]{6}$/i.test(chosen)) return chosen;
  return PALETTE[hash(name) % PALETTE.length];
}
