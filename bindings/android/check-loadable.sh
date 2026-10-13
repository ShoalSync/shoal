#!/usr/bin/env bash
# Fails if a library in the AAR given cannot be loaded by Android: one marked as needing static
# thread-local storage, which Android refuses in a library loaded at run time (0.17.0's was).
# Uses the NDK's llvm-readelf, so ANDROID_NDK_HOME must be set.
set -euo pipefail

aar=$1
readelf="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
unzip -q "$aar" 'jni/*' -d "$work"
status=0
found=0
for lib in "$work"/jni/*/*.so; do
  found=1
  if "$readelf" --dynamic "$lib" | grep -q STATIC_TLS; then
    echo "error: ${lib#"$work"/} needs static TLS, which Android will not load" >&2
    status=1
  fi
done
[ "$found" = 1 ] || { echo "error: no libraries in $aar" >&2; exit 1; }
exit "$status"
