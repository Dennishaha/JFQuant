#!/bin/bash
# Developer build script. Runtime app needs neither this script nor a compiler.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
native_root="${JFQUANT_NATIVE_BUILD_ROOT:-$repo_root/target/native-macos14}"
package_root="$repo_root/target/package-macos14"
bundle_root="$repo_root/target/distribution"
app="$bundle_root/JFQuant.app"
[ "$(uname -m)" = arm64 ] || { echo 'Apple Silicon build host required'; exit 1; }
# Replacing a running Python tree can remove its certificates and extensions mid-request.
# Refuse any process using this generated bundle; never kill or cancel user tasks.
require_bundle_idle() {
  while read -r process_id executable; do
    case "$executable" in
      "$app/Contents/"*)
        echo "bundle_in_use: process $process_id uses $executable. Wait for active tasks, stop idle services and close the App before packaging." >&2
        return 1
        ;;
    esac
  done < <(/bin/ps -axo pid=,comm=)
}
require_bundle_idle

"$repo_root/crates/app/packaging/build-collector.sh"
export MACOSX_DEPLOYMENT_TARGET=14.0
export CARGO_TARGET_DIR="$package_root"
# libduckdb-sys downloads its matching, pinned native release into the profile deps.
unset DUCKDB_LIB_DIR
export DUCKDB_DOWNLOAD_LIB=1
cargo build --locked --manifest-path "$repo_root/Cargo.toml" -p analytics-desktop -p analytics-cli --release
# Replace only the generated bundle, never a user workspace.
mkdir -p "$bundle_root"
require_bundle_idle
if [ -d "$app" ]; then rm -rf "$app"; fi
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources/Licenses"
cp "$package_root/release/analytics-desktop" "$package_root/release/jfquant" "$app/Contents/MacOS/"
cp -R "$repo_root/target/collector-build/python" "$app/Contents/Resources/python"
"$app/Contents/Resources/python/bin/python3" -I -B "$repo_root/crates/app/packaging/rewrite-python.py" "$app/Contents/Resources/python"
"$app/Contents/Resources/python/bin/python3" -I -B "$repo_root/crates/app/packaging/verify-python.py" "$app/Contents/Resources/python"
"$app/Contents/Resources/python/bin/python3" -I -B "$repo_root/crates/app/packaging/collect-python-licenses.py" "$app/Contents/Resources/Licenses/PythonPackages"
cp "$app/Contents/Resources/python/lib/python3.12/LICENSE.txt" "$app/Contents/Resources/Licenses/CPython.txt"
cp "$repo_root/crates/factor/QLIB_LICENSE.txt" "$app/Contents/Resources/Licenses/Qlib-Alpha158.txt"
cp "$repo_root/crates/app/packaging/Info.plist" "$app/Contents/Info.plist"
cp "$package_root/release/deps/libduckdb.dylib" "$app/Contents/Frameworks/"
cp "$native_root/openmp-20.1.8.src/LICENSE.TXT" "$app/Contents/Resources/Licenses/OpenMP.txt"
curl --fail --location --retry 3 --output "$app/Contents/Resources/Licenses/DuckDB.txt" https://raw.githubusercontent.com/duckdb/duckdb/v1.5.6/LICENSE
for binary in "$app/Contents/MacOS/"* "$app/Contents/Frameworks/"*.dylib; do
  chmod u+w "$binary"
  case "$binary" in *.dylib) install_name_tool -id "@rpath/$(basename "$binary")" "$binary";; esac
  while IFS= read -r dependency; do
    case "$dependency" in
      *lib_lightgbm.dylib|*libduckdb.dylib|*libomp.dylib)
        install_name_tool -change "$dependency" "@rpath/$(basename "$dependency")" "$binary";;
    esac
  done < <(otool -L "$binary" | awk '/compatibility version/{print $1}')
  while IFS= read -r rpath; do
    install_name_tool -delete_rpath "$rpath" "$binary"
  done < <(otool -l "$binary" | awk '/cmd LC_RPATH/{r=1;next}r && /path /{print $2;r=0}')
  install_name_tool -add_rpath '@executable_path/../Frameworks' "$binary"
  install_name_tool -add_rpath '@loader_path' "$binary"
done
"$repo_root/crates/app/packaging/verify-bundle.sh" "$app"
for binary in "$app/Contents/Frameworks/"*.dylib; do codesign --force --sign - "$binary"; done
# Sign all Python Mach-O objects before signing the enclosing bundle.
while IFS= read -r -d '' binary; do
  if file -b "$binary" | grep -q 'Mach-O'; then codesign --force --sign - "$binary"; fi
done < <(find "$app/Contents/Resources/python" -type f -print0)
codesign --force --sign - "$app/Contents/MacOS/jfquant"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
mkdir -p "$bundle_root/self-check"
env -i HOME="$bundle_root/self-check-home" PATH=/usr/bin:/bin "$app/Contents/MacOS/analytics-desktop" --self-check "$bundle_root/self-check" > "$bundle_root/self-check.json"
"$repo_root/crates/app/packaging/verify-cli.sh" "$app" "$bundle_root/cli-check"
"$repo_root/crates/app/packaging/verify-input-check.sh" "$app" "$bundle_root/input-check-offline"
"$repo_root/crates/app/packaging/verify-indices.sh" "$app" "$bundle_root/index-check"
# DMG contains only the app, install shortcut and source-controlled installation notes.
stage="$bundle_root/dmg-root"
if [ -d "$stage" ]; then rm -rf "$stage"; fi
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
cp "$repo_root/crates/app/packaging/INSTALL.txt" "$stage/"
hdiutil create -ov -volname JFQuant -srcfolder "$stage" -format UDZO "$bundle_root/JFQuant-macos14-arm64.dmg"
shasum -a 256 "$bundle_root/JFQuant-macos14-arm64.dmg" > "$bundle_root/JFQuant-macos14-arm64.dmg.sha256"
echo "App: $app"
echo "DMG: $bundle_root/JFQuant-macos14-arm64.dmg"
