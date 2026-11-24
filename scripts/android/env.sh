#!/bin/bash
# The cross-compile environment for one Android target (docs/ANDROID.md 5). Sourced, not run:
#
#   . scripts/android/env.sh <rust target>      aarch64-linux-android | x86_64-linux-android
#
# It sets what cargo, the `cc` crate and CMake-built C code (libopus) need to build with the NDK: the target's clang as
# compiler and linker, llvm-ar, the NDK's CMake toolchain file with Ninja. It exports ANDROID_ABI (arm64-v8a, x86_64)
# and ANDROID_LLVM_BIN (the NDK's tools, for llvm-readelf).
#
# Read from the environment, with these defaults:
#   ANDROID_HOME                 the SDK (default: %LOCALAPPDATA%/Android/Sdk on Windows, ~/Library/Android/sdk on macOS,
#                                ~/Android/Sdk elsewhere)
#   SWAPDESK_NDK_VERSION         28.1.13356709 (r28 or later: 16 KB pages by default)
#   SWAPDESK_ANDROID_API         30 (Android 11, the app's minimum)
#   SWAPDESK_CMAKE_VERSION       3.31.6 (the SDK's CMake, which ships Ninja)
#   SWAPDESK_ANDROID_TARGET_DIR  target/android under the repository (a short path helps CMake on Windows)

android_target="${1:?usage: . scripts/android/env.sh <rust target>}"
android_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    android_host=windows-x86_64
    android_exe=.exe
    android_cmd=.cmd
    android_native() { cygpath -m "$1"; }
    android_sdk_default="$(cygpath -m "${LOCALAPPDATA:-$HOME/AppData/Local}")/Android/Sdk"
    ;;
  Darwin)
    android_host=darwin-x86_64
    android_exe=
    android_cmd=
    android_native() { printf '%s' "$1"; }
    android_sdk_default="$HOME/Library/Android/sdk"
    ;;
  *)
    android_host=linux-x86_64
    android_exe=
    android_cmd=
    android_native() { printf '%s' "$1"; }
    android_sdk_default="$HOME/Android/Sdk"
    ;;
esac

android_sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$android_sdk_default}}"
android_ndk="$android_sdk/ndk/${SWAPDESK_NDK_VERSION:-28.1.13356709}"
android_api="${SWAPDESK_ANDROID_API:-30}"
export ANDROID_LLVM_BIN="$android_ndk/toolchains/llvm/prebuilt/$android_host/bin"

case "$android_target" in
  aarch64-linux-android) export ANDROID_ABI=arm64-v8a ;;
  x86_64-linux-android) export ANDROID_ABI=x86_64 ;;
  *)
    echo "env.sh: unknown target $android_target" >&2
    return 2
    ;;
esac

android_clang="$ANDROID_LLVM_BIN/$android_target$android_api-clang$android_cmd"
if [ ! -f "$android_clang" ]; then
  echo "env.sh: no compiler at $android_clang (is NDK ${SWAPDESK_NDK_VERSION:-28.1.13356709} installed in the SDK?)" >&2
  return 2
fi

android_upper="$(printf '%s' "$android_target" | tr 'a-z-' 'A-Z_')"
android_under="$(printf '%s' "$android_target" | tr '-' '_')"
export "CARGO_TARGET_${android_upper}_LINKER=$android_clang"
export "CC_${android_under}=$android_clang"
export "CXX_${android_under}=$ANDROID_LLVM_BIN/$android_target$android_api-clang++$android_cmd"
export "AR_${android_under}=$ANDROID_LLVM_BIN/llvm-ar$android_exe"
export ANDROID_NDK_HOME="$android_ndk" ANDROID_NDK_ROOT="$android_ndk" ANDROID_NDK="$android_ndk"
# C code built by CMake: the NDK's own toolchain file, with the Ninja the SDK's CMake ships.
android_cmake="$android_sdk/cmake/${SWAPDESK_CMAKE_VERSION:-3.31.6}/bin"
if [ -d "$android_cmake" ]; then
  case "$android_host" in
    windows-x86_64) PATH="$(cygpath -u "$android_cmake"):$PATH" ;;
    *) PATH="$android_cmake:$PATH" ;;
  esac
  export PATH
fi
export CMAKE_GENERATOR=Ninja
export "CMAKE_TOOLCHAIN_FILE_${android_under}=$android_ndk/build/cmake/android.toolchain.cmake"
export ANDROID_PLATFORM="android-$android_api"
export CARGO_TARGET_DIR="${SWAPDESK_ANDROID_TARGET_DIR:-$(android_native "$android_root")/target/android}"
