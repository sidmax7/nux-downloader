#!/usr/bin/env python3
"""Copies Tauri's bundles into one folder under release names.

Tauri names files after the product ("Nux Downloader_0.1.0_amd64.deb");
GitHub would turn the space into a dot, so release assets get plain,
predictable names instead. Signature files (`.sig`) follow their bundle.
Renaming keeps signatures valid: they cover the contents, not the name.

  collect-release-files.py <bundle dir> <version> <out dir>
"""

import shutil
import sys
from pathlib import Path

RULES = [
    (".AppImage", "nux-downloader_{v}_amd64.AppImage"),
    (".deb", "nux-downloader_{v}_amd64.deb"),
    (".rpm", "nux-downloader-{v}-1.x86_64.rpm"),
    (".dmg", "nux-downloader_{v}_universal.dmg"),
    (".app.tar.gz", "nux-downloader_{v}_universal.app.tar.gz"),
    ("-setup.exe", "nux-downloader_{v}_x64-setup.exe"),
]


def release_name(path: Path, version: str) -> str | None:
    name = path.name
    sig = name.endswith(".sig")
    if sig:
        name = name[: -len(".sig")]
    for suffix, pattern in RULES:
        if name.endswith(suffix):
            return pattern.format(v=version) + (".sig" if sig else "")
    return None


def main() -> None:
    bundle_dir, version, out = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    out.mkdir(parents=True, exist_ok=True)
    found = 0
    for path in sorted(bundle_dir.rglob("*")):
        if not path.is_file():
            continue
        target = release_name(path, version)
        if target:
            shutil.copy2(path, out / target)
            print(f"{path} -> {out / target}")
            found += 1
    if found == 0:
        sys.exit(f"no bundles found under {bundle_dir}")


if __name__ == "__main__":
    main()
