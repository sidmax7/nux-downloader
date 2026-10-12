// Fixtures for component tests.
import type { DraftDto, PackageDto, SnapshotDto } from "./bindings";

export function pkg(over: Partial<PackageDto> = {}): PackageDto {
  return {
    id: 1,
    name: "Movie pack",
    dest_dir: "/tmp/dl",
    priority: "normal",
    state: "downloading",
    error: null,
    extract: false,
    delete_archives: false,
    is_archive: false,
    limit: null,
    size: 1000,
    downloaded: 250,
    files: [
      {
        id: 10,
        name: "movie.mkv",
        state: "active",
        error: null,
        size: 1000,
        downloaded: 250,
        blocks: 4,
      },
    ],
    ...over,
  };
}

export function snapshot(over: Partial<SnapshotDto> = {}): SnapshotDto {
  return { packages: [], links: [], global_limit: null, engine_error: null, ...over };
}

export function draft(over: Partial<DraftDto> = {}): DraftDto {
  return {
    id: 1,
    name: "file",
    files: [{ source: "https://x/file.bin", name: "file.bin", size: 2048, error: null }],
    offers_extract: false,
    missing_volumes: [],
    ok_count: 1,
    ...over,
  };
}
