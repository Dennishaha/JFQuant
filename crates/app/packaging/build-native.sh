#!/bin/bash
# Build-only tooling; generated sources/libraries stay under ignored target/.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
build_root="${JFQUANT_NATIVE_BUILD_ROOT:-$repo_root/target/native-macos14}"
mkdir -p "$build_root"
command -v cmake >/dev/null || { echo 'Build prerequisite: cmake'; exit 1; }
# Pin source bytes as well as version tags; reject modified cached archives.
fetch_source() {
  local name="$1" version="$2" expected="$3" archive="$4"
  if [ ! -f "$archive" ]; then
    curl --fail --location --retry 3 --output "$archive.part" "https://github.com/llvm/llvm-project/releases/download/llvmorg-$version/$name-$version.src.tar.xz"
    mv "$archive.part" "$archive"
  fi
  [ "$(shasum -a 256 "$archive" | awk '{print $1}')" = "$expected" ] || { echo "$name source archive hash mismatch" >&2; exit 1; }
  if [ ! -d "$build_root/$name-$version.src" ]; then tar -xf "$archive" -C "$build_root"; fi
}
fetch_source openmp 20.1.8 b21c04ee9cbe56e200c5d83823765a443ee6389bbc3f64154c96e94016e6cee9 "$build_root/openmp.tar.xz"
fetch_source cmake 20.1.8 3319203cfd1172bbac50f06fa68e318af84dcb5d65353310c0586354069d6634 "$build_root/llvm-cmake.tar.xz"
# OpenMP's relative ../cmake discovery matches LLVM's source layout.
if [ ! -e "$build_root/cmake" ]; then ln -s cmake-20.1.8.src "$build_root/cmake"; fi
cmake -S "$build_root/openmp-20.1.8.src" -B "$build_root/openmp-build" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0 -DCMAKE_OSX_ARCHITECTURES=arm64 \
  -DCMAKE_INSTALL_PREFIX="$build_root/install" -DOPENMP_ENABLE_LIBOMPTARGET=OFF -DLIBOMP_ENABLE_SHARED=ON
cmake --build "$build_root/openmp-build" --parallel 4
cmake --install "$build_root/openmp-build"
echo "OpenMP: $build_root/install/lib/libomp.dylib"
