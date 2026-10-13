#!/bin/bash
set -euo pipefail
app="${1:?App bundle path required}"
for binary in "$app/Contents/MacOS/"* "$app/Contents/Frameworks/"*.dylib; do
  lipo -verify_arch arm64 "$binary"
  otool -L "$binary" | awk '/compatibility version/{print $1}' | while read -r dependency; do
    case "$dependency" in /usr/lib/*|/System/Library/*|@rpath/*|@loader_path/*|@executable_path/*) ;; *) echo "External dependency: $dependency"; exit 1;; esac
    case "$dependency" in @rpath/*) [ -f "$app/Contents/Frameworks/${dependency#@rpath/}" ] || { echo "Missing dependency: $dependency"; exit 1; };; esac
  done
  otool -l "$binary" | awk '/minos /{split($2,a,"."); if (a[1]>14) {print "Minimum OS exceeds 14: " $2; exit 1}}'
  if otool -l "$binary" | awk '/cmd LC_RPATH/{r=1;next}r && /path /{print $2;r=0}' | grep -E '^/'; then echo "Absolute build rpath: $binary"; exit 1; fi
done
