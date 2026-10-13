#!/bin/bash
# Build-only tooling; generated sources/libraries stay under ignored target/.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
build_root="${JFQUANT_NATIVE_BUILD_ROOT:-$repo_root/target/native-macos14}"
mkdir -p "$build_root"
command -v cmake >/dev/null || { echo 'Build prerequisite: cmake'; exit 1; }
if [ ! -d "$build_root/LightGBM/.git" ]; then
  git clone --depth 1 --branch v4.6.0 --recurse-submodules --shallow-submodules https://github.com/microsoft/LightGBM.git "$build_root/LightGBM"
fi
if [ ! -d "$build_root/openmp-20.1.8.src" ]; then
  curl --fail --location --retry 3 --output "$build_root/openmp.tar.xz" https://github.com/llvm/llvm-project/releases/download/llvmorg-20.1.8/openmp-20.1.8.src.tar.xz
  tar -xf "$build_root/openmp.tar.xz" -C "$build_root"
fi
# Standalone OpenMP 20 also needs LLVM's shared CMake modules.
if [ ! -d "$build_root/cmake-20.1.8.src" ]; then
  curl --fail --location --retry 3 --output "$build_root/llvm-cmake.tar.xz" https://github.com/llvm/llvm-project/releases/download/llvmorg-20.1.8/cmake-20.1.8.src.tar.xz
  tar -xf "$build_root/llvm-cmake.tar.xz" -C "$build_root"
fi
# OpenMP's relative ../cmake discovery matches LLVM's source layout.
if [ ! -e "$build_root/cmake" ]; then ln -s cmake-20.1.8.src "$build_root/cmake"; fi
cmake -S "$build_root/openmp-20.1.8.src" -B "$build_root/openmp-build" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0 -DCMAKE_OSX_ARCHITECTURES=arm64 \
  -DCMAKE_INSTALL_PREFIX="$build_root/install" -DOPENMP_ENABLE_LIBOMPTARGET=OFF -DLIBOMP_ENABLE_SHARED=ON
cmake --build "$build_root/openmp-build" --parallel 4
cmake --install "$build_root/openmp-build"
cmake -S "$build_root/LightGBM" -B "$build_root/lightgbm-build" \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0 -DCMAKE_OSX_ARCHITECTURES=arm64 \
  -DBUILD_CLI=OFF -DUSE_OPENMP=ON -DUSE_HOMEBREW_FALLBACK=OFF \
  -DOpenMP_CXX_FLAGS="-Xpreprocessor -fopenmp -I$build_root/install/include" \
  -DOpenMP_C_FLAGS="-Xpreprocessor -fopenmp -I$build_root/install/include" -DOpenMP_C_LIB_NAMES=omp \
  -DOpenMP_CXX_LIB_NAMES=omp -DOpenMP_omp_LIBRARY="$build_root/install/lib/libomp.dylib"
cmake --build "$build_root/lightgbm-build" --parallel 4
cp "$build_root/LightGBM/lib_lightgbm.dylib" "$build_root/install/lib/"
echo "Native libraries: $build_root/install/lib"
