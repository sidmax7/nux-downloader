#!/usr/bin/env bash
# Adds SHA256SUMS and the updater manifest (latest.json) to a GitHub
# release, computed from the files already on it. The release workflow's
# last job runs this; it can also be run by hand to redo that step:
#
#   bash scripts/finish-release.sh v0.1.0
#
# With DRY_RUN=1 it prints both files instead of uploading them.
set -euo pipefail

tag="${1:?usage: finish-release.sh <tag>}"
version="${tag#v}"
root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# `gh` finds the repository from the checkout, so it runs from there.
cd "$root"
gh release download "$tag" --dir "$work" --pattern '*'
rm -f "$work/SHA256SUMS" "$work/latest.json"

(cd "$work" && python3 "$root/scripts/update-manifest.py" "$version" "$tag" > latest.json)
(cd "$work" && files=(*) && sha256sum -- "${files[@]}" > SHA256SUMS)

if [ -n "${DRY_RUN:-}" ]; then
  cat "$work/latest.json" "$work/SHA256SUMS"
else
  gh release upload "$tag" "$work/SHA256SUMS" "$work/latest.json" --clobber
fi
