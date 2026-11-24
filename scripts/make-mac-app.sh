#!/bin/sh
# Mac 用の Lisa.app を作る。.lmf（マージ済みファイル）と .lmd（マージ済みフォルダ）を Lisa 独自の
# 種類（public.data にだけ適合。中身は JSON だが public.json やテキストには適合させないので、
# テキストエディタが既定の候補にならない）として書き出し、Lisa を持ち主（Owner）にする。
#
#   scripts/make-mac-app.sh [出力先のフォルダ] [--debug]
#
# 既定の出力先は target/release（--debug なら target/debug）。中の lisa と lisa-mcp は同じフォルダに
# 置く（LAN の共有で lisa が隣の lisa-mcp を立ち上げるため）。
set -eu
cd "$(dirname "$0")/.."

profile=release
out=""
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        -h|--help)
            echo "使い方: scripts/make-mac-app.sh [出力先のフォルダ] [--debug]" >&2
            exit 1
            ;;
        -*)
            echo "知らないオプション: $arg" >&2
            exit 1
            ;;
        *) out=$arg ;;
    esac
done

if [ "$profile" = release ]; then
    cargo build --release -p lisa -p lisa-mcp
else
    cargo build -p lisa -p lisa-mcp
fi
out=${out:-target/$profile}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
app="$out/Lisa.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "target/$profile/lisa" "target/$profile/lisa-mcp" "$app/Contents/MacOS/"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Lisa</string>
    <key>CFBundleDisplayName</key>
    <string>Lisa</string>
    <key>CFBundleIdentifier</key>
    <string>local.lisa.Lisa</string>
    <key>CFBundleExecutable</key>
    <string>lisa</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>UTExportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key>
            <string>local.lisa.merged-file</string>
            <key>UTTypeDescription</key>
            <string>Lisa のマージ済みファイル</string>
            <key>UTTypeConformsTo</key>
            <array>
                <string>public.data</string>
            </array>
            <key>UTTypeTagSpecification</key>
            <dict>
                <key>public.filename-extension</key>
                <array>
                    <string>lmf</string>
                </array>
            </dict>
        </dict>
        <dict>
            <key>UTTypeIdentifier</key>
            <string>local.lisa.merged-folder</string>
            <key>UTTypeDescription</key>
            <string>Lisa のマージ済みフォルダ</string>
            <key>UTTypeConformsTo</key>
            <array>
                <string>public.data</string>
            </array>
            <key>UTTypeTagSpecification</key>
            <dict>
                <key>public.filename-extension</key>
                <array>
                    <string>lmd</string>
                </array>
            </dict>
        </dict>
    </array>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Lisa のマージ済みファイル</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSHandlerRank</key>
            <string>Owner</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>local.lisa.merged-file</string>
            </array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Lisa のマージ済みフォルダ</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSHandlerRank</key>
            <string>Owner</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>local.lisa.merged-folder</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null
echo "$app を作りました"
