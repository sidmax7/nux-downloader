// One authored icon set: 24px grid, 1.75 stroke, round caps.
export const PATHS = {
  chevronRight: "m9 6 6 6-6 6",
  chevronDown: "m6 9 6 6 6-6",
  up: "M12 19V5M5 12l7-7 7 7",
  down: "M12 5v14M19 12l-7 7-7-7",
  close: "M18 6 6 18M6 6l12 12",
  pause: "M9 5v14M15 5v14",
  play: "M8 5.5v13l10.5-6.5z",
  trash: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3",
  plus: "M12 5v14M5 12h14",
  folder: "M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
  restart: "M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7",
  download: "M12 4v11M7 11l5 5 5-5M5 20h14",
  links: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1",
  sliders: "M4 7h9M17 7h3M4 17h3M11 17h9M15 4v6M9 14v6",
  wifi: "M5 12.5a10 10 0 0 1 14 0M8.5 16a5 5 0 0 1 7 0M12 19.5h.01",
  ethernet: "M8 3v5M16 3v5M6 8h12v4a6 6 0 0 1-12 0zM12 18v3",
  usb: "M12 3v14M9.5 5.5 12 3l2.5 2.5M12 12l4-2.5V8M12 14.5 8 12v-1.5M12 17a2 2 0 1 0 0 4 2 2 0 0 0 0-4",
  globe: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18",
  check: "m5 12.5 4.5 4.5L19 7.5",
  alert: "M12 4 3 20h18zM12 10v4M12 17h.01",
  archive: "M4 5h16v4H4zM6 9v10h12V9M10 13h4",
  clock: "M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16zM12 8v4l3 2",
  blocks: "M4 4h6v6H4zM14 4h6v6h-6zM4 14h6v6H4zM14 14h6v6h-6z",
} as const;

export type IconName = keyof typeof PATHS;
