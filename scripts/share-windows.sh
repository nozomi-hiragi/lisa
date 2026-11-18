#!/bin/sh
# Windows 用の lisa.exe と lisa-mcp.exe をこの PC（Mac）で作って、LAN の他の PC へ HTTP で渡す。
#
#   scripts/share-windows.sh [ポート]
#
# 初めてのときだけ、次の 2 つが要る:
#   brew install mingw-w64
#   rustup target add x86_64-pc-windows-gnu
set -eu
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-gnu
usage="使い方: scripts/share-windows.sh [--port ポート | ポート]"

port=""
if [ "$#" -eq 0 ]; then
    :
elif [ "$#" -eq 1 ]; then
    case "$1" in
        -h|--help)
            echo "$usage" >&2
            exit 1
            ;;
        --port=*)
            port=${1#--port=}
            ;;
        -*)
            echo "知らないオプション: $1" >&2
            echo "$usage" >&2
            exit 1
            ;;
        *)
            port=$1
            ;;
    esac
elif [ "$#" -eq 2 ] && { [ "$1" = "--port" ] || [ "$1" = "-p" ]; }; then
    port=$2
else
    echo "$usage" >&2
    exit 1
fi

if [ -n "$port" ]; then
    case "$port" in
        ''|*[!0-9]*)
            echo "ポートは数で: $port" >&2
            echo "$usage" >&2
            exit 1
            ;;
    esac
    if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
        echo "ポートは 1 から 65535 の間で指定してください: $port" >&2
        exit 1
    fi
    set -- --port "$port"
else
    set --
fi

if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "x86_64-w64-mingw32-gcc がありません。brew install mingw-w64 で入れてください。" >&2
    exit 1
fi
if ! rustup target list --installed | grep -qx "$target"; then
    echo "Rust の $target がありません。rustup target add $target で入れてください。" >&2
    exit 1
fi

# 中のデバッグ情報を落として小さくする（210 MB → 120 MB ほど）。
CARGO_PROFILE_RELEASE_STRIP=true cargo build --release --target "$target" -p lisa -p lisa-mcp
exec cargo run --release -p lisa-scan -- share \
    "target/$target/release/lisa.exe" \
    "target/$target/release/lisa-mcp.exe" \
    "$@"
