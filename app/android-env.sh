# Source before cross-compiling: `. ./android-env.sh`
#
# The NDK ships one clang wrapper per API level, not the bare triple cc-rs
# wants. API 26 is the floor for the WebView features wry uses.
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/30.0.16248370}"
export NDK_HOME="$ANDROID_NDK_HOME"

_TB="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
_API=26

export CC_aarch64_linux_android="$_TB/aarch64-linux-android$_API-clang"
export CXX_aarch64_linux_android="$_TB/aarch64-linux-android$_API-clang++"
export AR_aarch64_linux_android="$_TB/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$_TB/aarch64-linux-android$_API-clang"

export CC_armv7_linux_androideabi="$_TB/armv7a-linux-androideabi$_API-clang"
export AR_armv7_linux_androideabi="$_TB/llvm-ar"
export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$_TB/armv7a-linux-androideabi$_API-clang"

export PATH="$_TB:$PATH"
