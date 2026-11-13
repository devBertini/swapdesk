#!/usr/bin/env bash
# Runs every fuzz target of fuzz/ for SECONDS each (default 30) with cargo-fuzz on a nightly toolchain (D40).
# On Linux it runs here; on Windows run it inside a throwaway container from the repository root:
#   docker run --rm -v "$PWD":/src -v swapdesk-fuzz-target:/src/fuzz/target -v swapdesk-fuzz-cargo:/usr/local/cargo/registry \
#     -w /src rust:1-bookworm bash scripts/dev/fuzz.sh 30
# Prints FUZZ_OK <target> or FUZZ_CRASH <target> per target, and exits 1 if any target crashed.
set -u
SECONDS_EACH="${1:-30}"
cd "$(dirname "$0")/../../fuzz"
rustup toolchain install nightly --profile minimal >/dev/null 2>&1 || true
command -v cargo-fuzz >/dev/null 2>&1 || cargo install cargo-fuzz --locked >/dev/null 2>&1
failed=0
for target in $(cargo +nightly fuzz list); do
    mkdir -p "corpus/$target"
    if cargo +nightly fuzz run "$target" "corpus/$target" -- -max_total_time="$SECONDS_EACH" -rss_limit_mb=2048 >"/tmp/fuzz-$target.log" 2>&1; then
        echo "FUZZ_OK $target ($(grep -o 'Done [0-9]* runs' "/tmp/fuzz-$target.log" | tail -1))"
    else
        echo "FUZZ_CRASH $target"
        tail -40 "/tmp/fuzz-$target.log"
        failed=1
    fi
done
exit $failed
