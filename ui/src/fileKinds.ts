// Groups files by what they are, for the quick filters in the review step.

export type FileKind = "video" | "image" | "audio" | "archive" | "document" | "other";

/** In the order the filters are shown. */
export const KIND_ORDER: FileKind[] = ["video", "image", "audio", "archive", "document", "other"];

export const KIND_LABEL: Record<FileKind, string> = {
  video: "Videos",
  image: "Images",
  audio: "Audio",
  archive: "Archives",
  document: "Documents",
  other: "Other",
};

const EXTENSIONS: Record<Exclude<FileKind, "other">, string[]> = {
  video: ["mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "ts", "mpg", "mpeg", "3gp"],
  image: ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "avif", "svg", "ico"],
  audio: ["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma"],
  archive: ["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso", "img", "cab"],
  document: ["pdf", "epub", "mobi", "azw3", "cbz", "cbr", "djvu", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "txt"],
};

const BY_EXTENSION = new Map<string, FileKind>(
  Object.entries(EXTENSIONS).flatMap(([kind, list]) => list.map((ext) => [ext, kind as FileKind])),
);

/** The kind of a file, judged by its name. Split-archive volumes (`.001`, `.r00`) are archives. */
export function fileKind(name: string): FileKind {
  const dot = name.lastIndexOf(".");
  if (dot < 0) return "other";
  const ext = name.slice(dot + 1).toLowerCase();
  const known = BY_EXTENSION.get(ext);
  if (known) return known;
  if (/^r?\d{2,3}$/.test(ext) && ext.length === 3) return "archive";
  return "other";
}
