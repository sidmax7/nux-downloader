#!/usr/bin/env bash
# Real-world comparison for IMPLEMENTATION_PLAN.md P7.4: nuxd against a
# single curl (one big file) and against sequential downloading (a list of
# files). Needs a server that does not rate-limit requests per IP (see
# docs/benchmarks.md) and a built release binary:
#
#   cargo build --release -p nuxd
#   scripts/realworld-bench.sh single  URL  [LINKS]     # LINKS: eth0,wlan0
#   scripts/realworld-bench.sh multi   LIST [LINKS]     # LIST: one URL per line
#
# It prints wall-clock seconds and bytes for each tool; paste the result
# into docs/benchmarks.md with the date, links and server. Plexo has to be
# timed by hand on the same file and hardware (its UI shows the finish
# time); note its version next to the number.
set -euo pipefail

mode=${1:?usage: realworld-bench.sh single|multi URL_OR_LIST [LINKS]}
target=${2:?need a URL (single) or a list file (multi)}
links=${3:-}
nuxd=${NUXD:-target/release/nuxd}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

timeit() { # label, command...
  local label=$1; shift
  local start end
  start=$(date +%s.%N)
  "$@" >/dev/null 2>&1 || echo "  ($label exited non-zero)"
  end=$(date +%s.%N)
  local secs bytes
  secs=$(echo "$end - $start" | bc -l)
  bytes=$(du -sb "$work" | cut -f1)
  printf '| %-34s | %8.2f s | %14d B | %8.1f MB/s |\n' "$label" "$secs" "$bytes" \
    "$(echo "$bytes / $secs / 1000000" | bc -l)"
  rm -rf "${work:?}"/*
}

echo "| Run | Time | Bytes | Rate |"
echo "|---|---|---|---|"
nuxd_links=()
[ -n "$links" ] && nuxd_links=(--links "$links")

case $mode in
  single)
    timeit "curl, one connection" curl -sSL -o "$work/file" "$target"
    timeit "nuxd, all links" "$nuxd" get "$target" -o "$work"
    if [ -n "$links" ]; then
      timeit "nuxd, $links" "$nuxd" get "${nuxd_links[@]}" "$target" -o "$work"
      first=${links%%,*}
      timeit "nuxd, $first only" "$nuxd" get --links "$first" "$target" -o "$work"
    fi
    ;;
  multi)
    timeit "curl, one file after another" bash -c \
      "while read -r u; do [ -n \"\$u\" ] && curl -sSL -O --output-dir '$work' \"\$u\"; done < '$target'"
    timeit "nuxd, parallel queue" bash -c \
      "'$nuxd' --data-dir '$work/.state' add -i '$target' -o '$work' && '$nuxd' --data-dir '$work/.state' resume"
    ;;
  *) echo "mode must be single or multi" >&2; exit 2 ;;
esac
