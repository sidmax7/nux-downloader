#!/usr/bin/env python3
"""Prints one version's section of CHANGELOG.md (for release notes).

  changelog-section.py <version>
"""

import re
import sys
from pathlib import Path


def main() -> None:
    version = sys.argv[1]
    text = Path(__file__).resolve().parent.parent.joinpath("CHANGELOG.md").read_text()
    match = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)", text, re.M | re.S)
    if match and match.group(1).strip():
        print(match.group(1).strip())
    else:
        print(f"See CHANGELOG.md for what changed in {version}.")


if __name__ == "__main__":
    main()
