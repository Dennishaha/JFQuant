#!/bin/bash
set -euo pipefail
app="${1:?App bundle path required}"
for binary in "$app/Contents/MacOS/"* "$app/Contents/Frameworks/"*.dylib; do
  lipo -verify_arch arm64 "$binary"
  otool -L "$binary" | awk '/compatibility version/{print $1}' | while read -r dependency; do
    case "$dependency" in /usr/lib/*|/System/Library/*|@rpath/*|@loader_path/*|@executable_path/*) ;; *) echo "External dependency: $dependency"; exit 1;; esac
    case "$dependency" in @rpath/*) [ -f "$app/Contents/Frameworks/${dependency#@rpath/}" ] || { echo "Missing dependency: $dependency"; exit 1; };; esac
  done
  otool -arch arm64 -l "$binary" | awk '
    /cmd LC_BUILD_VERSION/{v=1}
    /cmd LC_VERSION_MIN_MACOSX/{v=2}
    (v==1 && /minos /) || (v==2 && /^[[:space:]]*version /) {
      split($2,a,"."); seen=1; v=0;
      if (a[1]>14 || (a[1]==14 && (a[2]>0 || a[3]>0))) {print "Minimum OS exceeds 14.0: " $2; exit 1}
    }
    END {if (!seen) {print "Missing minimum OS version"; exit 1}}'
  if otool -l "$binary" | awk '/cmd LC_RPATH/{r=1;next}r && /path /{print $2;r=0}' | grep -E '^/'; then echo "Absolute build rpath: $binary"; exit 1; fi
done

"$app/Contents/Resources/python/bin/python3" -I -B "$(dirname "$0")/verify-python.py" "$app/Contents/Resources/python"
env -i HOME=/tmp PATH=/usr/bin:/bin "$app/Contents/Resources/python/bin/python3" -I -B -c 'import akshare,yfinance,jfquant_collectors; assert akshare.__version__=="1.19.1"; assert yfinance.__version__=="1.7.0"'
