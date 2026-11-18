#!/bin/sh
# Windows 用の lisa.exe と lisa-mcp.exe をこの PC（Mac）で作って、ペアリング済みの Windows へ LAN 越しに入れ替える。
#
#   scripts/update-windows.sh [名前か device_id] [--target lisa|lisa-mcp|both] [--no-restart]
#
# 初めてのときだけ、次の 2 つが要る:
#   brew install mingw-w64
#   rustup target add x86_64-pc-windows-gnu
set -eu
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-gnu
usage="使い方: scripts/update-windows.sh [名前か device_id] [--target lisa|lisa-mcp|both] [--no-restart]"

peer_set=0
i=1
while [ "$i" -le "$#" ]; do
    eval "arg=\${$i}"
    case "$arg" in
        -h|--help)
            echo "$usage" >&2
            exit 1
            ;;
        --no-restart)
            i=$((i + 1))
            ;;
        --target|-t)
            if [ "$i" -ge "$#" ]; then
                echo "--target に lisa、lisa-mcp、both のどれかが要ります" >&2
                exit 1
            fi
            i=$((i + 1))
            eval "val=\${$i}"
            case "$val" in
                lisa|lisa-mcp|both) ;;
                *)
                    echo "--target は lisa、lisa-mcp、both のどれかにしてください: $val" >&2
                    exit 1
                    ;;
            esac
            i=$((i + 1))
            ;;
        --target=*)
            val=${arg#--target=}
            case "$val" in
                lisa|lisa-mcp|both) ;;
                *)
                    echo "--target は lisa、lisa-mcp、both のどれかにしてください: $val" >&2
                    exit 1
                    ;;
            esac
            i=$((i + 1))
            ;;
        -*)
            echo "知らないオプション: $arg" >&2
            echo "$usage" >&2
            exit 1
            ;;
        *)
            if [ "$peer_set" -eq 0 ]; then
                peer_set=1
                i=$((i + 1))
            else
                echo "$usage" >&2
                exit 1
            fi
            ;;
    esac
done

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
exec cargo run --release -p lisa-scan -- remote-update "$@"
