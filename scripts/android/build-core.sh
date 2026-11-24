#!/bin/bash
# Builds the Android app's Rust library for each ABI and puts it where Gradle packs it (docs/ANDROID.md 5):
#
#   bash scripts/android/build-core.sh [--release] [arm64-v8a] [x86_64]      (default: both ABIs, a development build)
#
# A development build has the cargo feature `dev-harness` (the self-test, the probe and the scripted viewer) and goes
# to android/app/src/debug/jniLibs/<abi>/, which only the debug APK packs. A release build has no harness, uses the
# release profile that unwinds, and goes to android/app/src/release/jniLibs/<abi>/, which only the release APK packs:
# neither APK can carry the other's library.
#
# For each ABI: `cargo build -p swapdesk-app-android --target <rust target>` with the NDK (scripts/android/env.sh),
# the library copied, and read back:
#   - every LOAD segment is aligned to 16 KB (0x4000), which Google Play requires of apps that target Android 15 or
#     later;
#   - a release library exports no symbol of the development harness.
# The exit code is the first failure's.
set -u
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root" || exit 2
export PATH="$HOME/.cargo/bin:$PATH"

profile=debug
variant=debug
flags=(--features dev-harness)
abis=()
for arg in "$@"; do
  case "$arg" in
    --release)
      # The release profile that unwinds: a panic must become a Java exception, not end the app's process (D15).
      profile=release-service
      variant=release
      flags=(--profile release-service)
      ;;
    arm64-v8a | x86_64) abis+=("$arg") ;;
    *)
      echo "build-core.sh: unknown argument $arg" >&2
      exit 2
      ;;
  esac
done
[ "${#abis[@]}" -gt 0 ] || abis=(arm64-v8a x86_64)

for abi in "${abis[@]}"; do
  case "$abi" in
    arm64-v8a) target=aarch64-linux-android ;;
    x86_64) target=x86_64-linux-android ;;
  esac
  # A subshell per ABI: each target has its own compiler variables.
  (
    . scripts/android/env.sh "$target" || exit 2
    rustup target add "$target" >/dev/null 2>&1
    echo "== cargo build -p swapdesk-app-android --target $target ${flags[*]}"
    cargo build -p swapdesk-app-android --target "$target" "${flags[@]}" || exit 1
    built="$CARGO_TARGET_DIR/$target/$profile/libswapdesk_android.so"
    [ -f "$built" ] || { echo "build-core.sh: no library at $built" >&2; exit 1; }
    out="android/app/src/$variant/jniLibs/$abi"
    mkdir -p "$out"
    cp "$built" "$out/libswapdesk_android.so" || exit 1
    readelf="$ANDROID_LLVM_BIN/llvm-readelf"
    [ -x "$readelf" ] || readelf="$readelf.exe"
    # 16 KB pages: every loadable segment's alignment (the last column of a LOAD line) is 0x4000 or more.
    aligns="$("$readelf" -lW "$out/libswapdesk_android.so" | awk '$1 == "LOAD" { print $NF }' | sort -u | tr '\n' ' ')"
    echo "== $abi ($variant): LOAD alignment $aligns($(wc -c <"$out/libswapdesk_android.so") bytes)"
    [ -n "$aligns" ] || { echo "build-core.sh: no LOAD segment read from the library" >&2; exit 1; }
    for a in $aligns; do
      [ "$((a))" -ge 16384 ] || { echo "build-core.sh: $abi has a LOAD segment aligned to $a, below 16 KB" >&2; exit 1; }
    done
    harness="$("$readelf" --dyn-syms -W "$out/libswapdesk_android.so" | grep -c "DevHarness")"
    if [ "$variant" = release ] && [ "$harness" -ne 0 ]; then
      echo "build-core.sh: the release library of $abi exports $harness symbol(s) of the development harness" >&2
      exit 1
    fi
    echo "== $abi ($variant): $harness harness symbol(s)"
  ) || exit $?
done
