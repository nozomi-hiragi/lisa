# Lisa

複数PCのファイルをハッシュで管理・整理するファイラー（開発中）

フォルダの中身をタイルにして小さく入れ子に描き、押したフォルダへズームして入っていく
「フラクタル」な見た目のファイラーです。

- フォルダの中身は、フォルダとファイルをまとめて名前順に、左上から横へタイルにして並べます。
  スクロールはせず、全部をフォルダの中に収めます。列の数は、タイルができるだけ大きく、余るマスが
  少なくなるように選びます。
- タイルは 1 つの枠で、フォルダなら上に名前の入った耳（タブ）と本体（A4 の横長、√2:1）、
  ファイルなら上にプレビュー（4:3）と下に名前が入ります。耳の横幅は本体の 1/3 が最小で、名前が
  長ければ伸びます。
- 耳と名前の大きさはズームに合わせて変わり、一覧の中でも開いたときでも読める大きさになります。
  耳や名前が大きくなった分は、本体やプレビューが縮んで譲ります。
- 開いたものは枠ごと画面に収め、周りには同じ階層の隣が見切れて見えます。

## いまの状態

v0.1.0 の開発段階です。この端末のドライブを裏で深さ優先で読み、SQLite（`lisa.db`）に保存し、中身を BLAKE3 でハッシュして重複の一覧（`dups`）やドライブごとのスナップショットを作ります。また、同じ LAN にある他の PC の Lisa を見つけてペアリングし、相手のドライブやフォルダを自分の木の中で見たり、MCP（`lisa-mcp`）から調べたりできます（[docs/design.md](docs/design.md)、[docs/schema.md](docs/schema.md)、[docs/scan.md](docs/scan.md)）。`LISA_DUMMY=1` で起動すると、ダミーの木（端末 3 台、数階層、500 ファイルのフォルダを含む）を出します。

## 必要なもの

- [Rust](https://www.rust-lang.org/ja/tools/install)（`rustup` でインストール）
- macOS / Windows（トラックパッドの細かい操作は macOS 向けに作っています）

## ビルドと起動

```sh
git clone git@github.com:nozomi-hiragi/lisa.git
cd lisa

cargo run              # ビルドして起動（この端末のドライブを読む）
LISA_DUMMY=1 cargo run # ダミーの木で起動（何も読まない）
cargo build            # ビルドのみ
cargo build --release  # リリースビルド（target/release/lisa）
cargo test --workspace # テスト（スキャンの crate も）

# スキャンや DB の確認を CLI で試す（Bevy をビルドしないので速い）
cargo run -p lisa-scan -- ls ~/Documents     # 1 段だけ読んで一覧を出す
cargo run -p lisa-scan -- tree ~ 3           # 3 段下までたどって数と速さを出す
cargo run -p lisa-scan -- drives             # つながっているドライブの一覧と ID
cargo run -p lisa-scan -- dups               # アプリの DB から重複の一覧を出す
cargo run -p lisa-scan -- snapshots          # アプリの DB からスナップショットの一覧を出す
```

- 初回のビルドは、依存している Bevy のコンパイルに十数分かかります。2 回目以降は数秒です。
- フォント（`assets/fonts/NotoSansJP-Light.otf`）は実行ファイルに埋め込んであるので、
  `target/release/lisa` を直接起動しても、他の PC へコピーしても文字が出ます。
- `lisa-scan` はファイルの中身を読みません（名前、大きさ、日時だけ）。Mac では、クラウドにだけある
  ファイルを読んでもダウンロードしない設定にしてから動きます。`~/Documents` などを初めて読むときは、
  起動した Terminal にアクセスの許可を聞かれます。

## 他の PC へ渡す（Mac で Windows 用を作る）

Mac で Windows 用の `lisa.exe` と `lisa-mcp.exe` を作り、同じ LAN の PC へ HTTP で渡せます。

```sh
# 初めてのときだけ
brew install mingw-w64
rustup target add x86_64-pc-windows-gnu

scripts/share-windows.sh        # ビルドして http://<この Mac>:8765/ で渡す（Ctrl+C で止める）
scripts/share-windows.sh 9000   # ポートを変える
```

- 表示された URL を Windows のブラウザで開いて落とすか、PowerShell で表示された
  `curl.exe -o lisa.exe http://...`（と `curl.exe -o lisa-mcp.exe http://...`）を打って落とします。
- ブラウザで落とすと、署名のない exe なので SmartScreen が「PC が保護されました」と止めます。
  「詳細情報」から「実行」で起動できます。`curl.exe` で落とした場合は止められません。
- Mac のファイアウォールが受信を許可するか聞いてきたら、許可してください。
- 渡すだけなら、どのファイルでも `cargo run -p lisa-scan -- share <ファイル>... [--port N | N]` で渡せます。
  合言葉も暗号化もないので、家など信頼できる LAN でだけ使ってください。

## 操作

| 操作 | マウス | トラックパッド | キーボード |
| --- | --- | --- | --- |
| 見えているフォルダ・ファイルへ移る（中の子でも、隣や親の隣でも） | 左クリック | クリック / ピンチで広げる | |
| 親へ戻る（ズームアウト） | 右クリック | ピンチで縮める | Esc |
| 拡大縮小（ポインタを中心に） | ホイール | ピンチ | |
| 移動 | 中ボタンで引っぱる | 2本指で動かす | |
| 同じ階層の隣のタイルへ | 中ボタンで引っぱる | 2本指で動かす（斜めも可） | |
| 動きを止めてつかむ | | 2本指を置く | |
| 設定を開く・閉じる | メニューバーの「設定」 | | Esc で閉じる |
| FPS 表示の切り替え（デバッグビルド） | | | F3 |

- ピンチは、使った幅で行き先を決めます。少し（約 1.2 倍）広げて離せば 1 段内側へ、少し縮めて
  離せば 1 段外側へ移ります。子のプレビューが小さくても、大きくズームする必要はありません。
- 中身が多いフォルダでは、全体と 1 つの間に、画面の短い辺にタイルがちょうど何個か収まる段が
  細かくあります。ピンチした分だけ近い段に止まり、スクロールするとタイルの切れ目で止まります。
- 2 本指で動かして離すと、その勢いの先にある同じ階層のタイルへ寄って止まります。速く払えば、
  少なくとも隣へ 1 つ進みます。
- 開いている場所は、動いている途中でもいちばん近いものへ切り替わります。動いている途中で指を
  置くと、その場でつかめます。
- 隣のタイルがある範囲は縦も横も斜めも重さなしで動けます。行き先の無い方向は重くなります
  （ラバーバンド）。
- 詳しい決まりは [docs/input.md](docs/input.md) にあります。

## LAN の他の PC の Lisa を見る

同じ LAN にある複数の PC で Lisa を動かすと、他の PC のドライブやフォルダを自分の木の `Root` の下に「端末」として並べて見られます。木で見るときは、相手の PC のファイルを読んだり動かしたりはせず、相手の DB を読むだけです（ただしペアリングで発行する鍵には、下の「Mac から LAN 越しに更新・起動・終了する」にある通り、アプリの起動・終了や実行ファイルの入れ替えの権限も含まれます。信頼できる自分の端末だけを許可してください）。

1. **見せる側の PC で公開を入れる**: メニューバーの「設定」を開き、「LAN に公開」を「公開する」にします（HTTP 47800・UDP 47801 と、別プロセスの `lisa-mcp` 47802 で待ちます。「しない」に戻せば sidecar も含めて止まります）。
2. **見る側の PC で開いて許可してもらう**: 起動時（と 30 秒ごと）に LAN の他の Lisa を自動で見つけ、`Root` の下に端末のタイルを出します。初めての端末を開くと 4 桁の番号が出るので、相手の PC の画面に出たポップアップ（「`○○ がつなぎたいと言っています。番号 1234（許可すると、ファイルの一覧を見る・アプリを起動し直す・更新することができます）`」）で同じ番号を確かめて「許可」を押してください。
3. **2 回目から**: 許可された鍵を `peers.json` に覚えるので、次からは黙ってつながります。許可を取り消したいときは、見せる側の「設定」の「つないだ端末」で「取り消す」を押します。

- Windows や Mac のファイアウォールが受信を許可するか聞いてきたら、プライベートネットワーク（家庭内 LAN）での受信（TCP 47800 / UDP 47801 / TCP 47802）を許可してください。
- Mac から相手につなぐには、macOS の「システム設定 → プライバシーとセキュリティ → ローカルネットワーク」で、Lisa を起動したアプリ
  （ターミナルなど）を許可しておく必要があります。許可が無いと、相手が動いていても `No route to host`（os error 65）やタイムアウトになります
  （Apple の `curl` は許可が無くても届くので、`curl` では届くのに Lisa だけ届かないときはこれを疑ってください）。
- 通信は暗号化されない平文の HTTP です。家など信頼できる LAN の中だけで使ってください。

## MCP で使う

`lisa-mcp` は、Claude などの MCP クライアントから Lisa を使うためのサーバーです（標準入出力、または HTTP）。
ユーザーのファイルやフォルダは何も動かしません（ペアリングした端末からだけ使える `lisa_update` で、`lisa` / `lisa-mcp` 自身の実行ファイルを入れ替えることはできます）。

```sh
cargo build --release -p lisa-mcp
claude mcp add lisa -- "$PWD/target/release/lisa-mcp"   # Claude Code に足す
```

### LAN の他の PC の Lisa を使う

相手の PC で `lisa-mcp --listen` を動かすと、MCP の Streamable HTTP（`POST /mcp`、JSON の応答のみ）で待ちます。
起動すると、LAN のアドレスと合言葉入りの `claude mcp add` コマンドが標準エラーに出るので、自分の PC でそれを打ちます。

```sh
# 1. 相手の PC で動かす（既定は 0.0.0.0:47800。アドレスやポートを変えるなら --listen 0.0.0.0:9000）
lisa-mcp --listen

# 2. 自分の PC で、相手に表示された 1 行を打つ
claude mcp add --transport http lisa-<名前> http://<IP>:47800/mcp --header "Authorization: Bearer <合言葉>"
```

- Windows や Mac のファイアウォールが受信を許可するか聞いてきたら、プライベートネットワーク（家庭内 LAN）での受信を許可してください。
- 通信は暗号化されない平文の HTTP です。家など信頼できる LAN の中だけで使ってください。
- 合言葉は、初回の起動時に作って設定と同じデータのフォルダの `lan.json`（macOS は `~/Library/Application Support/Lisa/lan.json`、Windows は `%APPDATA%\Lisa\data\lan.json`）に保存します。合言葉を変えたいときは、`lan.json` を消して起動し直すか、`--token <文字列>` で指定してください（`--token` を指定したときは、その合言葉だけが通り、`lan.json` に保存されている合言葉は通りません）。

| 道具 | 内容 | アプリ |
| --- | --- | --- |
| `lisa_drives` | 読んだドライブの一覧 | 要らない |
| `lisa_list` | フォルダの中身（1 段、名前順か大きさ順） | 要らない |
| `lisa_info` | 1 つの情報（ハッシュ、同じ中身の数、フォルダの中の合計） | 要らない |
| `lisa_search` | 名前で探す | 要らない |
| `lisa_duplicates` | 重複の一覧（大きさや場所で絞れる） | 要らない |
| `lisa_copies` | 同じ中身の場所を全部 | 要らない |
| `lisa_snapshots` | ドライブごとのスナップショット | 要らない |
| `lisa_logs` | アプリのログ（`logs/lisa.log`、`lisa.1.log`）と `crash.log` の最後の行 | 要らない |
| `lisa_app_state` | 開いているフォルダ、重複の一覧、読み込みの進み具合 | 起動中 |
| `lisa_app_show` | その場所へ画面を飛ばす | 起動中 |
| `lisa_app_duplicates` | 重複の一覧を開け閉めし、組を選んで光らせる | 起動中 |
| `lisa_app_rescan` | フォルダを先に読み直してもらう | 起動中 |
| `lisa_app_pause` | 裏の読み込みの一時停止・再開 | 起動中 |
| `lisa_update` | `lisa` または `lisa-mcp` の実行ファイルを HTTP で落として blake3 を確かめ、入れ替える | 要らない（ペアリングした端末からだけ） |
| `lisa_app_start` | 同じフォルダのアプリを起動し、5 秒待ってまだ生きているか返す（`env` で描き方の環境変数を足せる、`restart: true` で再起動） | 要らない（ペアリングした端末からだけ） |
| `lisa_app_stop` | アプリに終了を頼み、返事が無いときや `force: true` のときはプロセスを強制終了する | 要らない（ペアリングした端末からだけ） |
| `lisa_app_alive` | アプリのプロセスが生きているかと、`control.json` に返事があるかを返す | 要らない（ペアリングした端末からだけ） |
| `lisa_screenshot` | OS の画面（または Lisa の窓）を連続で撮って 24 ビット BMP（base64）で返す | 要らない（ペアリングした端末からだけ） |

- DB を読む道具は、アプリの DB（`~/Library/Application Support/Lisa/lisa.db`）を読むだけです。
  アプリが起動していなくても使えます。パスは Mac の `/Users/…` のままで引けます。
- `lisa_app_*` は、起動中のアプリに頼みます。アプリは起動すると `127.0.0.1` の空いている番号で待ち、
  番号と合言葉（起動ごとに作る）を DB と同じフォルダの `control.json` に書きます（自分だけ読める）。
  合言葉の合わない頼みは断ります。
- `lisa_update`、`lisa_app_start`、`lisa_app_stop`、`lisa_app_alive`、`lisa_screenshot` は、ペアリングした端末の鍵（`peers.json` の `key`）で来たときだけ使えます（合言葉 token や標準入出力では使えません）。

### 落ちたときに Mac から調べる

Windows のリリースビルド（`lisa.exe`）はコンソールの窓を出さないため、落ちても画面には何も出ないことがあります。
アプリは起動するたびにログをデータのフォルダ（Windows は `%APPDATA%\Lisa\data`、macOS は `~/Library/Application Support/Lisa`）の `logs/lisa.log`（1 つ前は `logs/lisa.1.log`）に書き、panic で止まったときは `crash.log` にも書き足します。

設定の「LAN に公開」が「公開する」になっていれば、アプリは起動してすぐ（ウィンドウを作るより前に）同じフォルダの `lisa-mcp`（Windows は `lisa-mcp.exe`）を `0.0.0.0:47802` で別プロセスとして立ち上げます。アプリが落ちても `lisa-mcp` はそのあと 10 分間動き続けるので、一度ペアリングしてある Mac からそのままログと `crash.log` を読めます。

```sh
# ペアリング済みの相手が 1 台だけなら引数は要りません
cargo run -p lisa-scan -- remote-logs

# 相手が複数あるときや、行数を変えるとき（既定は最後の 200 行）
cargo run -p lisa-scan -- remote-logs <名前か device_id> 500
```

- `lisa.exe` と同じフォルダに `lisa-mcp.exe` を置いておいてください（`scripts/share-windows.sh` は両方を渡します）。
- Windows や macOS のファイアウォールが `lisa-mcp` の受信（TCP 47802）を許可するか聞いてきたら、家庭内 LAN での受信を許可してください（許可の窓が出たままになっていると、応答が返らず待ち続けます）。
- 47802 につながらないときは、アプリ本体の口（47800）も試します。

#### Mac から LAN 越しに更新・起動・終了する（ペアリングした端末からだけ）

Windows で `lisa-mcp.exe`（47802）が動いていれば、ペアリング済みの Mac からブラウザを使わずに `lisa.exe` と `lisa-mcp.exe` を入れ替えたり、アプリの起動・終了・生死確認ができます。これらの操作は、ペアリングした端末の鍵（`peers.json` の `key`）で来たときだけ使えます（合言葉 token では使えません）。

- **最初の 1 回だけ**: 今 Windows で動いている古い `lisa-mcp.exe` にはまだ `lisa_update` などの道具がありません。最初の 1 回だけは、タスクマネージャー（または開いているコンソール）で動いている古い `lisa-mcp.exe`（と `lisa.exe`）を止め、`scripts/share-windows.sh` などで新しい `lisa.exe` と `lisa-mcp.exe` に手で置き換えてから起動してください。2 回目からは Mac からそのまま入れ替えられます。

```sh
# ビルドして、ペアリング済みの Windows の lisa.exe と lisa-mcp.exe を順番に入れ替えて再起動する
scripts/update-windows.sh
scripts/update-windows.sh <名前か device_id> --target lisa --no-restart

# ビルド済みのバイナリをそのまま渡して入れ替える（ビルドはしない）
cargo run -p lisa-scan -- remote-update
cargo run -p lisa-scan -- remote-update <名前か device_id> --target lisa
cargo run -p lisa-scan -- remote-update --target lisa-mcp
cargo run -p lisa-scan -- remote-update --no-restart

# アプリの生死確認・起動・終了（固まっているときは --force で強制終了）
cargo run -p lisa-scan -- remote-app alive
cargo run -p lisa-scan -- remote-app start
cargo run -p lisa-scan -- remote-app stop
cargo run -p lisa-scan -- remote-app <名前か device_id> stop --force

# 描き方を変えて起動し直す（ちらつきや GPU まわりの切り分け）
cargo run -p lisa-scan -- remote-app <名前か device_id> start --restart --env LISA_UPDATE_MODE=continuous
cargo run -p lisa-scan -- remote-app <名前か device_id> start --restart --env LISA_PRESENT_MODE=fifo --env LISA_UPDATE_MODE=continuous
cargo run -p lisa-scan -- remote-app <名前か device_id> start --restart --env WGPU_BACKEND=gl

# OS の画面（既定は Lisa の窓だけ、--full で画面全体）を連続で撮って /tmp/lisa-shots/<時刻>/ に保存する
cargo run -p lisa-scan -- remote-screenshot <名前か device_id>
cargo run -p lisa-scan -- remote-screenshot <名前か device_id> --count 8 --interval 60 --scale 2 --full
```

- `start` の `--env KEY=VALUE` で渡せる環境変数は次の 4 つだけです（値は 64 文字まで、英数字と `_,=:.-` だけ）。選んだ値は起動時の `logs/lisa.log` の 1 行目（`backend=...`, `present_mode=...`, `update_mode=...`）に出ます。
  - `LISA_UPDATE_MODE`: `reactive`（既定、止まっている間は 1 秒に 1 回だけ描く）か `continuous`（いつも描く）
  - `LISA_PRESENT_MODE`: `auto_vsync`（既定）、`auto_no_vsync`、`fifo`、`fifo_relaxed`、`mailbox`、`immediate`
  - `WGPU_BACKEND`: `gl`（Windows の既定）、`dx12`、`vulkan` など
  - `RUST_LOG`: ログの絞り込み（例: `info,wgpu=warn`）

#### まだペアリングしていない・設定を変える前に落ちるとき（手で `lisa-mcp.exe --listen` を動かす）

まだ一度もペアリングできていないときや、「LAN に公開」を入れる前に落ちてしまうときは、Windows で `lisa-mcp.exe --listen` を手で動かしておけば、Mac の Claude から `lisa_logs` でログと `crash.log` を読めます。

1. `scripts/share-windows.sh` で `lisa.exe` と `lisa-mcp.exe` を Windows へ渡します。
2. Windows の PowerShell やコマンドプロンプトで `.\lisa-mcp.exe --listen` を動かしておきます（ショートカットのプロパティの「リンク先」の末尾に ` --listen` を付けてダブルクリックしても、`lisa-mcp.exe` は通常のコンソールアプリなので黒いコンソールの窓が開き、そこに URL と合言葉入りの `claude mcp add ...` が見えたまま待ち続けます）。
3. Windows のファイアウォールが受信を許可するか聞いてきたら、プライベートネットワーク（家庭内 LAN）での受信を許可します。
4. 表示された `claude mcp add --transport http ...` を Mac で 1 回打てば、以後は Windows で `lisa.exe` を起動して落ちたあと、Mac の Claude から `lisa_logs` を呼んで `logs/lisa.log`（起動時の版・OS・exe の場所・`data_dir`・管理者権限の有無を含む）や `lisa.1.log`、`crash.log` を読めます。

## 設定

メニューバーの「設定」で変えられます。値は次のファイルに保存され、起動時に読み込みます。
項目が足りない古いファイルでも、その項目だけ既定値で読みます。

- macOS: `~/Library/Application Support/Lisa/settings.json`
- Windows: `%APPDATA%\Lisa\config\settings.json`

| 項目 | 既定 | 範囲 | 内容 |
| --- | --- | --- | --- |
| 耳の高さ（開いたとき） | 28 px | 12〜64 px（2 刻み） | 開いたフォルダの耳の、画面上の高さ |
| 耳の高さ（一覧の中） | 16 px | 6〜48 px（2 刻み） | 一覧の中のフォルダの耳（とファイルの名前）の、親を開いたときの画面上の高さ。タイルが小さいときは、タイルの高さの 40 % まで。開いた状態から一覧の大きさまで縮めるあいだに、開いたときの高さからこの高さへ変わる。耳が大きくなった分は本体が縮んで譲る |
| 裏で読む速さの上限 | 200 MB/秒 | 0〜2000 MB/秒（50 刻み、0 は上限なし） | 裏で読む・ハッシュする速さの上限（見ているところは除く） |
| 電池のときは裏で読まない | 休む | 休まない / 休む | 電池で動いているときに裏の読み込み・ハッシュを休むか |
| CPU が混んだら裏で読まない | 80 % 超 | 20〜100 %（10 刻み、100 % は休まない） | CPU の混み具合がこれを超えたら裏の分を休む |
| ディスクが混んだら裏で読まない | 100 MB/秒超 | 0〜1000 MB/秒（25 刻み、0 は休まない） | ほかのアプリのディスクの読み書きがこれを超えたら裏の分を休む |
| LAN に公開 | しない | しない / 公開する | LAN の他の PC にこの端末を公開するか（HTTP 47800・UDP 47801・sidecar 47802） |

## デバッグ

- デバッグビルドでは、ウィンドウのタイトルに FPS とフレーム数を出します（F3 で切り替え）。
  止まっているときは約 1 fps になります。
- トラックパッドの入力の記録（デバッグビルドのみ）:

  ```sh
  LISA_INPUT_LOG=1 cargo run
  ```

  `/tmp/lisa-input.log` に、生の入力（macOS の NSEvent の指の段階・慣性の段階・動き）と
  状態の移り変わりを書き足します。おかしな動きを再現したら、このファイルからテストを作れます。

## 既知の課題

- **日本語・中国語の改行位置データ（ICU4X のセグメンテーションモデル）を同梱していないため、
  その警告を止めている。** 今はラベルを折り返さないので表示には影響しない。ラベルの折り返しを
  使うようになったら、Bevy の `complex_script_segmentation` 機能（`bevy_text` → `parley` の
  `complex-scripts`）を有効にして改行位置データを入れ、`src/main.rs` の `LogPlugin` の絞り込み
  （`icu_provider=off`）と、`Cargo.toml` の `icu_provider` の `logging` 機能を外すこと。
- ズームしている途中は、子フォルダをキャッシュした画像（1024×724 px）を拡大して見せるので、
  少しぼやける。開いている場所がその子に切り替わると、本物の描画になる。
- 名前の並びは文字コード順で、日本語の読みの順ではない（「夏」が「春」より先など）。
- 重なった場面は、z の大小ではなく作った順に描かれる（後に作ったほうが上）。上に見せたい
  場面は後から作ること（[docs/architecture.md](docs/architecture.md)）。
- 中身が多いフォルダは、全体ではタイルが小さくなって見分けにくい（500 ファイルの「スキャン」など）。
  今は少しピンチすれば、タイルがちょうど収まる段で読める。
  **あとでジーニースケール（カーソルの近くのタイルだけを大きく見せる、Dock の拡大のような
  見せ方）を入れる予定。** 詳しくは [docs/architecture.md](docs/architecture.md) の「今後」。
- 本物のトラックパッドでの手ざわり（重さ、勢い、吸い付きの強さ）は、まだ調整中。
- Windows では OpenGL で描く（`src/main.rs` の `render_plugin`）。Intel Iris Graphics 550 の PC で、
  Vulkan だと起動の直後に panic もなく落ち、DirectX 12 だと画面がちらついたうえ、しばらくすると GPU を
  見失って落ちたため。OpenGL では Bevy の SSAO が作れないので、使わない形で組み込む。ほかの描き方を
  試したいときは、環境変数 `WGPU_BACKEND`（`dx12`、`vulkan`、`gl` など）で変えられる。

## ドキュメント

- [docs/architecture.md](docs/architecture.md) — ソースの構成、描画の流れ、画像キャッシュ、LAN 共有、描画頻度
- [docs/input.md](docs/input.md) — 入力の扱い（2 本指スクロールの状態機械、ラバーバンド、離したあとの動き、ピンチで止まる段）
- [docs/design.md](docs/design.md) — ハッシュでの管理の考え方と仕組み
- [docs/schema.md](docs/schema.md) — 保存するデータの形（一部実装）
- [docs/scan.md](docs/scan.md) — 実際のスキャンに要るものと、実装の順番（一部実装）
- [docs/catalog.md](docs/catalog.md) — フォルダをアプリと結びつけるカタログ（計画）

## ライセンス

フォント（Noto Sans JP）は SIL Open Font License 1.1 です（`assets/fonts/OFL.txt`）。
