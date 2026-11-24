#!/bin/sh
# Mac 用の Lisa.app を作る。.lmf（マージ済みファイル）と .lmd（マージ済みフォルダ）を Lisa 独自の
# 種類（public.data にだけ適合。中身は JSON だが public.json やテキストには適合させないので、
# テキストエディタが既定の候補にならない）として書き出し、Lisa を持ち主（Owner）にする。
# .app として起動すると macOS の「ローカルネットワーク」の許可が要る（初回に確認が出る。
# 許可するまで LAN の他の PC につながらない）。
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
    <key>NSLocalNetworkUsageDescription</key>
    <string>LAN の他の PC の Lisa を見つけ、ペアリングした PC のファイルを一覧したり、重複の整理を中継したりするために使います。</string>
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

# 署名: ad-hoc のままだと、作り直すたびに macOS が別のアプリと見なし、ローカルネットワークや
# ファイアウォールの許可がやり直しになる。そこで、利用者のキーチェーンに置いたローカルの自己署名の
# コード署名証明書（無ければここで作る）で常に署名し、「識別子＋証明書」で同じアプリと見なさせる。
# 秘密鍵はリポに置かない。Lisa 専用のキーチェーン（~/Library/Keychains/lisa-codesign.keychain-db）に
# 入れ、その鍵束のパスワードは乱数で作ってログインキーチェーンに保管する（署名のたびに確認を出さない）。
# 別の証明書を使うときは LISA_SIGN_IDENTITY にその名前を渡す。
identity=${LISA_SIGN_IDENTITY:-Lisa Local Signing}
keychain=""
if [ -z "${LISA_SIGN_IDENTITY:-}" ]; then
    keychain="$HOME/Library/Keychains/lisa-codesign.keychain-db"
    service=lisa-codesign-keychain
    if [ ! -f "$keychain" ]; then
        echo "署名用の証明書「${identity}」を作ります（${keychain}）"
        password=$(openssl rand -hex 24)
        security add-generic-password -U -a "$USER" -s "$service" -w "$password"
        security create-keychain -p "$password" "$keychain"
        security set-keychain-settings "$keychain"
        work=$(mktemp -d)
        # 鍵と p12 は途中で失敗しても必ず消す。
        trap 'rm -rf "$work"' EXIT INT TERM
        cat > "$work/req.cnf" <<CNF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$identity
[v3]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
CNF
        openssl req -x509 -newkey rsa:3072 -nodes -days 7300 -config "$work/req.cnf" \
            -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null
        p12pass=$(openssl rand -hex 16)
        openssl pkcs12 -export -legacy -inkey "$work/key.pem" -in "$work/cert.pem" \
            -out "$work/id.p12" -passout "pass:$p12pass" 2>/dev/null \
            || openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" \
                -out "$work/id.p12" -passout "pass:$p12pass"
        security unlock-keychain -p "$password" "$keychain"
        security import "$work/id.p12" -k "$keychain" -P "$p12pass" -T /usr/bin/codesign >/dev/null
        security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$keychain" >/dev/null
        rm -rf "$work"
        trap - EXIT INT TERM
    fi
    password=$(security find-generic-password -a "$USER" -s "$service" -w)
    security unlock-keychain -p "$password" "$keychain"
fi
sign() {
    if [ -n "$keychain" ]; then
        codesign --force --keychain "$keychain" -s "$identity" "$@"
    else
        codesign --force -s "$identity" "$@"
    fi
}
# 中の実行ファイルも同じ証明書で、固定の識別子で署名する（--deep は使わない）。
sign --identifier local.lisa.Lisa.mcp "$app/Contents/MacOS/lisa-mcp"
sign "$app"
codesign --verify --strict "$app/Contents/MacOS/lisa-mcp"
codesign --verify --strict "$app"
codesign -d -r- "$app" 2>&1 | sed -n 's/^designated => /署名の要件: /p'
echo "$app を作りました"
