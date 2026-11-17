#!/bin/sh
# Windows 用の lisa.exe をこの PC（Mac）で作って、LAN の他の PC へ HTTP で渡す。
#
#   scripts/share-windows.sh [ポート]
#
# 初めてのときだけ、次の 2 つが要る:
#   brew install mingw-w64
#   rustup target add x86_64-pc-windows-gnu
set -eu
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-gnu

if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "x86_64-w64-mingw32-gcc がありません。brew install mingw-w64 で入れてください。" >&2
    exit 1
fi
if ! rustup target list --installed | grep -qx "$target"; then
    echo "Rust の $target がありません。rustup target add $target で入れてください。" >&2
    exit 1
fi

# 中のデバッグ情報を落として小さくする（210 MB → 120 MB ほど）。
CARGO_PROFILE_RELEASE_STRIP=true cargo build --release --target "$target"
exec cargo run --release -p lisa-scan -- share "target/$target/release/lisa.exe" "$@"
