#!/bin/bash
# Runs cargo nextest with the given args; when Smart App Control blocks a fresh test exe (os error 4551), deletes that
# exe, touches its crate so it relinks, and retries (up to 8 times). Development helper for this PC only.
export PATH="$HOME/.cargo/bin:$PATH"
for i in 1 2 3 4 5 6 7 8; do
  out=$(cargo nextest run "$@" 2>&1); code=$?
  exe=$(echo "$out" | grep -B4 "os error 4551" | grep -o "[A-Za-z]:[^']*deps.[^']*[.]exe" | head -1)
  build=$(echo "$out" | grep -B4 "os error 4551" | grep -o "[A-Za-z]:[^\`']*build-script-build" | head -1)
  if [ -z "$exe$build" ]; then echo "$out"; exit $code; fi
  if [ -n "$exe" ]; then
    echo "SAC bloqueou $exe" >&2; rm -f "$exe"
    name=$(basename "$exe" .exe); name=${name%-*}
    f=$(ls crates/*/tests/$name.rs crates/${name//_/-}/src/lib.rs 2>/dev/null | head -1)
    [ -n "$f" ] && touch "$f" || touch crates/*/src/lib.rs
  else
    echo "SAC bloqueou $build" >&2; rm -r "$(dirname "$build")"
  fi
done
echo "$out"; exit $code
