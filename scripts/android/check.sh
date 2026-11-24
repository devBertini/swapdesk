#!/bin/bash
# The Android gate (docs/ANDROID.md): what the desktop gate cannot see, since the `*-android` crates compile to nothing
# off Android. It is not part of the desktop gate or of its time budget.
#
#   bash scripts/android/check.sh [--apk]
#
# 1. `cargo clippy` of every `*-android` crate for both Android targets, warnings denied, without and with the
#    development harness;
# 2. scripts/android/build-core.sh: the development library for both ABIs, its 16 KB alignment read back;
# 3. with --apk: `gradlew assembleDebug` (JAVA_HOME names a JDK 17 or later, ANDROID_HOME the SDK). Gradle runs without
#    a daemon (android/gradle.properties), so the build leaves no process behind.
# The exit code is the first failure's.
set -u
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root" || exit 2
export PATH="$HOME/.cargo/bin:$PATH"

apk=0
for arg in "$@"; do
  case "$arg" in
    --apk) apk=1 ;;
    *)
      echo "check.sh: unknown argument $arg" >&2
      exit 2
      ;;
  esac
done

packages=()
for dir in crates/*-android; do
  packages+=(-p "$(basename "$dir")")
done

for target in aarch64-linux-android x86_64-linux-android; do
  (
    . scripts/android/env.sh "$target" || exit 2
    rustup target add "$target" >/dev/null 2>&1
    echo "== cargo clippy ${packages[*]} --target $target"
    cargo clippy "${packages[@]}" --target "$target" --locked -- -D warnings || exit $?
    # The development build's own code (the harness) is linted too.
    echo "== cargo clippy -p swapdesk-app-android --features dev-harness --target $target"
    cargo clippy -p swapdesk-app-android --features dev-harness --target "$target" --locked -- -D warnings
  ) || exit $?
done

bash scripts/android/build-core.sh || exit $?

if [ "$apk" -eq 1 ]; then
  echo "== gradlew assembleDebug"
  sh android/gradlew -p android assembleDebug || exit $?
fi
echo "== android check passed"
