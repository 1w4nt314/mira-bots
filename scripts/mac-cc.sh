#!/bin/sh
# C compiler wrapper for the Linux -> aarch64-apple-darwin cross-check (plan7 A.10, research7 §5).
# Used as CC_aarch64_apple_darwin by `npm run check:mac` / `npm run clippy:mac`. cc-rs passes
# Apple-only flags (-arch X, -mmacosx-version-min=...) that Linux clang does not know; drop them
# and target darwin explicitly. Only build scripts that compile header-free C/ObjC need this
# (objc2-exception-helper); nothing is linked. Not needed on a Mac (CI uses Apple's clang).
if ! command -v clang >/dev/null 2>&1; then
  echo "mac-cc: clang mangler (apt install clang)" >&2
  exit 1
fi
skip=0
for a in "$@"; do
  shift
  if [ "$skip" = 1 ]; then
    skip=0
    continue
  fi
  case "$a" in
    -arch) skip=1 ;;
    -mmacosx-version-min=*) ;;
    *) set -- "$@" "$a" ;;
  esac
done
exec clang --target=arm64-apple-macos11 "$@"
