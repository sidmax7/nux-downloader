// A colour for every link, stable across runs: the user's choice, else a
// palette entry picked from the interface name.

const PALETTE = [
  "#2f6fed",
  "#e0603a",
  "#2aa876",
  "#b45be0",
  "#d9a31c",
  "#16a2c4",
  "#d94f8c",
  "#7a8b2e",
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
