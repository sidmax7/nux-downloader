#!/usr/bin/env bash
# Runs every cargo-fuzz target (IMPLEMENTATION_PLAN.md P8.1) for a while.
#
#   cargo install cargo-fuzz --locked     # once
#   scripts/fuzz-all.sh [SECONDS_PER_TARGET]    # default 3600
#
# cargo-fuzz wants a nightly compiler for the sanitizer flags; on a stable
# toolchain setting RUSTC_BOOTSTRAP=1 (done below) is enough. A crash is
# left in fuzz/artifacts/<target>/; reproduce with
#   RUSTC_BOOTSTRAP=1 cargo fuzz run <target> fuzz/artifacts/<target>/<file>
# and turn the input into a unit test next to the code it broke.
set -euo pipefail
cd "$(dirname "$0")/../fuzz"
secs=${1:-3600}
export RUSTC_BOOTSTRAP=1
status=0
for target in headers grouping entry_paths manifests text_parsers; do
  echo "== $target ($secs s)"
  cargo fuzz run "$target" -- -max_total_time="$secs" -rss_limit_mb=2048 || status=1
done
exit $status
