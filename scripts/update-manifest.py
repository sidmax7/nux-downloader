#!/usr/bin/env python3
"""Prints the updater manifest (latest.json) for a release.

Run inside a folder holding the release assets (with their `.sig` files).
Only the formats that update themselves are listed: the AppImage, the
macOS app archive (one universal build serves both architectures) and the
Windows installer.

  update-manifest.py <version> <tag>
"""

import datetime
import json
import subprocess
import sys
from pathlib import Path

REPO = "https://github.com/sidmax7/nux-downloader"


def main() -> None:
    version, tag = sys.argv[1], sys.argv[2]
    entries = {
        "linux-x86_64": f"nux-downloader_{version}_amd64.AppImage",
        "darwin-aarch64": f"nux-downloader_{version}_universal.app.tar.gz",
        "darwin-x86_64": f"nux-downloader_{version}_universal.app.tar.gz",
        "windows-x86_64": f"nux-downloader_{version}_x64-setup.exe",
    }
    platforms = {}
    for platform, name in entries.items():
        sig = Path(f"{name}.sig")
        if not sig.is_file():
            sys.exit(f"missing {sig}: was TAURI_SIGNING_PRIVATE_KEY set for the build?")
        platforms[platform] = {
            "signature": sig.read_text().strip(),
            "url": f"{REPO}/releases/download/{tag}/{name}",
        }
    root = Path(__file__).resolve().parent.parent
    notes = subprocess.run(
        [sys.executable, str(root / "scripts/changelog-section.py"), version],
        capture_output=True,
        text=True,
        cwd=root,
    ).stdout.strip()
    manifest = {
        "version": version,
        "notes": notes,
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
