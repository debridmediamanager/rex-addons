#!/usr/bin/env bash
# Builds librexaddons.so for the Android app: the rex-addons binary, named as Android packages
# an executable, stripped, with 16 KB aligned LOAD segments, one per ABI.
#
#   ./build-android.sh                 # arm64-v8a armeabi-v7a x86_64 into target/android/<abi>/
#   ABIS=arm64-v8a OUT=../app/src/main/jniLibs ./build-android.sh
set -euo pipefail
cd "$(dirname "$0")"
ndk=${ANDROID_NDK_HOME:-${ANDROID_HOME:-$HOME/Library/Android/sdk}/ndk/28.2.13676358}
case "$(uname -s)" in Darwin) host=darwin-x86_64 ;; Linux) host=linux-x86_64 ;; *) echo 'Build on Linux or macOS' >&2; exit 1 ;; esac
prebuilt="$ndk/toolchains/llvm/prebuilt/$host"
toolchain="$prebuilt/bin"
[ -x "$toolchain/clang" ] || { echo "No NDK at $ndk: set ANDROID_NDK_HOME" >&2; exit 1; }
out=${OUT:-target/android}
target_dir=${CARGO_TARGET_DIR:-target}
# rquickjs ships no bindings for Android; the `android` feature generates them with bindgen,
# which needs a libclang the build machine can load (the NDK's, unless LIBCLANG_PATH says).
# On Linux that libclang does not find its own headers (stdbool.h), so name its resource dir.
export LIBCLANG_PATH=${LIBCLANG_PATH:-$prebuilt/lib}
for abi in ${ABIS:-arm64-v8a armeabi-v7a x86_64}; do
    case "$abi" in
        arm64-v8a) triple=aarch64-linux-android clang=aarch64-linux-android ;;
        # Rust names the 32-bit target armv7, the NDK's compiler armv7a.
        armeabi-v7a) triple=armv7-linux-androideabi clang=armv7a-linux-androideabi ;;
        x86_64) triple=x86_64-linux-android clang=x86_64-linux-android ;;
        *) echo "Unknown ABI: $abi" >&2; exit 1 ;;
    esac
    rustup target list --installed | grep -qx "$triple" || rustup target add "$triple"
    linker="$toolchain/${clang}26-clang"
    upper=$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')
    env "CC_${triple//-/_}=$linker" "AR_${triple//-/_}=$toolchain/llvm-ar" \
        "CARGO_TARGET_${upper}_LINKER=$linker" \
        "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-arg=-Wl,-z,max-page-size=16384" \
        "BINDGEN_EXTRA_CLANG_ARGS_${triple//-/_}=--target=${clang}26 --sysroot=$prebuilt/sysroot -resource-dir=$("$toolchain/clang" -print-resource-dir)" \
        cargo build --quiet --release --locked --features android --target "$triple"
    so="$out/$abi/librexaddons.so"
    mkdir -p "$(dirname "$so")"
    "$toolchain/llvm-strip" --strip-unneeded -o "$so" "$target_dir/$triple/release/rex-addons"
    "$toolchain/llvm-readelf" -lW "$so" | awk '/LOAD/ { if ($NF != "0x4000" && $NF != "0x10000") { print "Invalid ELF alignment: " $0; bad = 1 } } END { exit bad }'
    # The store build carries no service-specific code, so no service's name either.
    if "$toolchain/llvm-strings" "$so" | grep -qiE 'spotify|librespot'; then echo "$so names spotify or librespot" >&2; exit 1; fi
    echo "Built $so ($(wc -c <"$so" | tr -d ' ') bytes)"
done
