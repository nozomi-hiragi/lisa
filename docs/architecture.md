# 構成

Lisa は [Bevy](https://bevyengine.org/) 0.20 の上で動く 2D のアプリです。ここではソースの構成と、
描画の流れ、守るべき決まりをまとめます。

## ソースの構成

```
src/
  main.rs                  アプリの組み立て（プラグインの登録、ログの絞り込み、Windows の OpenGL 既定）
  crash.rs                 落ちたときの処理（crash.log への記録、Windows の知らせの窓）
  model.rs
  model/tree.rs            ファイルの木（読んだところだけ。この端末とドライブ、読めたら子を足す。ダミーの木）
  layout.rs                木の配置（名前順のタイル、列の数の選び方、入れ子の枠、基準のフォルダの周りだけ）
  render.rs
  render/scene.rs          1 つのフォルダの「場面」を描く（タイル、子の枠）
  render/ear.rs            フォルダの耳とファイルの名前（ズームに合わせて大きさを変える）
  render/shapes.rs         四角・三角・円・画像・文字を描く道具、見えないほど小さいものを省く
  render/cache.rs          開いたフォルダの中身を画像に撮って残すキャッシュ（`CONTENT_LAYER` のみ 1 枚撮影・WebP 保存）
  render/diag.rs           描画状態の診断ログ（`LISA_RENDER_DIAG=1` で 1 秒ごとにカメラ・スワップチェーン等を記録）
  render/status.rs         読み込みの状態ごとのタイルの見た目（枠だけ、脈打つ光、暗い膜、×）
  render/tag.rs            カタログの札、役割で暗くする膜、パネルで選んだ当たりの枠（本物の場面だけ）
  preview.rs               ファイルのプレビューの仕組み（Cartridge トレイト、PreviewRegistry、紙の背景）
  preview/cartridges.rs    組み込みカートリッジの一覧（1 行足す・消すだけで有効・無効）
  preview/cartridges/      種類ごとのカートリッジ（image、text、video、other）
  scan.rs                  本物のフォルダを裏のスレッドで読んで木に足す（DB からの出し入れと監視）
  scan/worker.rs           裏のスレッドでフォルダの読み取りとハッシュを進める係
  dups.rs                  重複の一覧を裏で作り、選んだ組のコピーを木の上で光らせる
  catalog.rs               カタログを裏で DB に当て、札と役割の印を決める、仕分けを書く、重複の一覧を並べ直す
  catalog/lan.rs           LAN の相手のカタログの当たりを読み、相手の木に印を付ける、相手の型を取り込む
  organize.rs              整理の計画を裏で 2 秒ごとに見張り、許可された計画を実行・取り消し・期限切れの掃除をする
  view.rs                  開いている場所（Nav）、場面の切り替え、クリック、飛ぶ、並べ直し
  view/camera.rs           枠に合わせる位置と倍率、ズームのアニメーション、描画頻度
  view/fade.rs             場面のフェード
  view/title.rs            ウィンドウのタイトルとメニューバーの場所の表示
  input.rs
  input/gesture.rs         ピンチ、ホイール、2 本指、中ボタン。指で動かしている間（ラバーバンド）
  input/settle.rs          指を離したあと（勢い、行き先へ寄せる、開いている場所の切り替え）
  input/zoom_levels.rs     ピンチで止まる段と、フォルダの中を拡大して見るときのタイルの切れ目
  input/scroll_input.rs    2 本指スクロールの状態機械（指・離した・慣性）とそのテスト
  input/mac_scroll.rs      macOS: NSEvent のスクロールを直接見るモニタ
  ui.rs
  ui/menu.rs               メニューバーと設定のポップアップ（Bevy UI）
  ui/progress.rs           読み込み中・ハッシュ中のリスト（画面左下）
  ui/dups.rs               重複の一覧のパネル（画面右側）
  ui/catalog.rs            カタログのパネル（画面右側。当たり、今のフォルダの仕分け、大きな未分類、LAN の相手）
  ui/organize.rs           整理の許可ポップアップ、移動先選びの案内、履歴のパネル（画面右側）
  ui/tooltip.rs            タイルにカーソルを止めると出す情報（ツールチップ）
  ui/context_menu.rs       タイルを右クリックしたときのメニュー（開く、親へ戻る、コピー、表示、移動、削除）
  settings.rs              設定（settings.json への保存と、ポップアップの項目）
  debug.rs                 デバッグビルドで FPS をタイトルに出す
  remote.rs                外（lisa-mcp）からの頼みを受けて画面を動かす（飛ぶ、重複の一覧、読み直し、一時停止）
  lan.rs                   LAN の他の PC の Lisa を見つけ、ペアリングして、木に出す（HTTP 待ち受け・UDP 発見・MCP 呼び出し・sidecar 管理）
crates/lisa-core/          スキャンの中身（Bevy に依存しない）
  src/db.rs                SQLite への保存と読み出し（device、drive、entry、tree、snapshot、meta）
  src/dups.rs              同じハッシュのファイル・フォルダをまとめた重複の一覧とキャッシュの署名
  src/fsevents.rs          Mac: FSEvents のイベント番号から前回以降に変わったフォルダを取る
  src/hash.rs              ファイルの BLAKE3 ハッシュと、フォルダの Merkle ハッシュ
  src/listing.rs           フォルダを 1 段読む（種類、大きさ、日時、リンク先、クラウドのみ、別のボリューム）
  src/name.rs              名前（ディスク上のバイト列、NFC、フォルダのハッシュと同じ並び、OS のゴミ）
  src/pace.rs              裏で読むときの I/O 優先度、速さの上限、電池・CPU・ディスクの混み具合の判定
  src/cloud.rs             Mac: クラウドにだけあるファイルを読んでもダウンロードしない設定
  src/volumes.rs           つながっているドライブの一覧（ID、名前、ファイルシステム、容量、外付けか）
  src/query.rs             DB を外から読む問い合わせ（パスから道のり、名前で探す、同じハッシュ）
  src/catalog.rs           カタログの型の読み込み（TOML、組み込みは build.rs で埋め込む）、札の文字と色、設定
  src/catalog/pattern.rs   パスのパターン（glob、**、段ごとの re:、NFC、大文字小文字）
  src/catalog/content.rs   中身の条件（content、content_bytes、hash、size、head）
  src/catalog/eval.rs      木（Tree）と中身の読み方（Reader）に対する当てはめと役割
  src/catalog/db.rs        DB の木への当てはめ、catalog_match、content_check、assignment、大きな未分類
  src/organize.rs          整理の計画・確かめ・隔離・実行・取り消し（organize/{plan,check,quarantine,apply}.rs）
  src/remote.rs            起動中のアプリを外から動かす口（127.0.0.1 と control.json、1 行の JSON）
crates/lisa-scan/          lisa-core を Bevy なしで試す CLI
  src/main.rs              ls、tree、drives、dups、snapshots、catalog、plan、remote-logs、remote-update、remote-app
  src/plan.rs              整理の計画の CLI（create、list、show、approve、reject、undo、purge）
  src/share.rs             LAN の他の PC へファイルを HTTP で渡す簡易サーバー
crates/lisa-mcp/           MCP のサーバー（標準入出力と Streamable HTTP。DB を読む道具と、アプリを動かす・更新する道具）
  src/main.rs              起動オプション（--listen、--token、--parent、--linger）と待ち受け
  src/lib.rs               MCP の道具一式、認証、ペアリング、sidecar 管理、バイナリ更新
  src/catalog.rs           カタログの道具（一覧、当たり、未分類、形の要約、試す、保存、仕分け）
  src/organize.rs          整理の道具（計画作成、状態・待機、一覧、取り下げ、取り消し。許可は作らない）
docs/                      このドキュメント
assets/fonts/              Noto Sans JP と、そのライセンス
```

モジュールは `foo.rs` と `foo/` を並べる書き方（Rust 2018 以降の形）でそろえています。
テストは、テストするコードと同じファイルに置いています。

## 1 フレームの流れ

`view.rs` の `TreeViewPlugin` が、毎フレーム次の順に動かします（`.chain()`）。

1. `check_sync_triggers` — 耳の高さ設定変更・木の増分変化・孤立検知を調べて `SyncRequest` を立て、開いたフォルダが変わったら `SceneVisit` を進める
2. `sync_view` — コア主導・GUI 主導の同期口（`Reset` / `RelayoutKeepView` / `RedrawScene`）。`Location` から `Nav`・`Layouts`・`CurrentScene`・`MainCamera`・パンくずを一括でそろえる（操作中は保留）
3. `reanchor` — 開いている場面のフォルダが `Layouts::anchor` と違うまま止まったら、そのフォルダを基準にして座標・カメラ・場面をそろえ直す
4. `navigate` — クリック、Esc、メニューの「開く」「親へ戻る」（`Location` と `Nav` を同時に書き換え、`Animating` を始める）
5. `gesture` — トラックパッドとマウスのジェスチャー、離したあとの動き（`refocus` / `focus_node` で `Location` と `Nav` を同時に書き換える）
6. `refit_on_resize` / `follow_side_panel` — ウィンドウの大きさや右の一覧の幅が変わったら合わせ直す
7. `animate_camera` — クリックや Esc で移るときのズームのアニメーション
8. `update_ears` — カメラの倍率に合わせて耳の大きさを直す
9. `fade_scenes` — 場面のフェード
10. `cache::update_thumbnails` — 読み戻し結果・ディスク読込の反映、見えている子フォルダの WebP 読み込み、開いたフォルダが落ち着いたら `CONTENT_LAYER` を 1 枚撮って保存
11. `pulse_loading` — 読み込み中のタイルの光を脈打たせる（光があるあいだは描き続ける）
12. `keep_redrawing` — 動いている間だけ、次のフレームを頼む
13. `update_title` — タイトルの書き換え（変わったときだけ）

メニューの処理（`MenuSystems`）、木を読む処理（`scan.rs`）、LAN の処理（`lan.rs` の `sync_share_listener`、`poll_discovery`、`poll_incoming_pairs`、`poll_worker_events`、`request_remote_nodes`、`update_lan_ui`）は、これより先に動きます。ただし操作中（`Animating` / `Fade` / `Gesture::active`）は、`scan.rs` と `lan.rs` の木への反映・読み込み要求・`JumpTo` を保留し、操作が止まってから木に当てて `sync_view` でそろえます。

## 状態の正と不変条件（書き換え箇所と同期の決まり）

開いている場所の「正」は `view::Location`（手元は DB の `entry.id` の道のり `Local(Vec<i64>)`、それ以外は Root からの名前の道のり `Named(Vec<String>)`）の 1 つだけです。`Nav.stack`（`NodeId` の列）は、`Location` を現在の `FileTree` に解決した派生ビューとして扱います。

### 書き換え箇所の一覧

| 状態 | 書き換える場所 |
| --- | --- |
| `FileTree`（構造変更） | `scan.rs` の `receive`（`Changed` で `reset_children`）・`load_visible`、`lan.rs` の `poll_discovery`・`poll_worker_events`（401 時の `reset_unauthorized_device`）・`request_remote_nodes`。いずれも操作中（`Animating` / `Fade` / `Gesture::active`）は受け取りを保留する。 |
| `Location`（正の場所） | コア主導: `scan.rs` の `resolve_jump`（`JumpTo` → `Location::Local`＋`SyncMode::Reset`）、`view.rs` の `sync_view`（解決後の最深ノードへ補正）。GUI 主導: `view.rs` の `navigate`、`input/settle.rs` の `switch_focus`・`focus_node`。 |
| `Nav` | `view.rs` の `setup`・`sync_view`（`Reset`）・`navigate`、`input/settle.rs` の `switch_focus`・`focus_node`。`scan.rs` は `Nav` を直接切り詰めない。 |
| `Layouts` | `view.rs` の `setup`・`sync_view`（`Reset` / `RelayoutKeepView`）・`reanchor`。 |
| `CurrentScene`（レイヤー 0） | `view.rs` の `setup`・`sync_view`・`reanchor`・`change_scene`、`view/camera.rs` の `animate_camera`（`then_show` 時のフェードイン）、`input/settle.rs` の `show_parent_if_zoomed_out`・`switch_focus`・`focus_node`。 |
| `Gesture::parent_scene` | `input/settle.rs` の `show_parent_if_zoomed_out`・`switch_focus`・`focus_node`・`coast_step`、`view.rs` の `navigate`・`sync_view`（`Reset` / `RelayoutKeepView` / `RedrawScene`）。`Gesture` の外へ漏らさない。 |
| `Fade` | 付与: `view.rs` の `change_scene`（`fade_in: false`）・`view/camera.rs` の `animate_camera`（`fade_in: true, replaces: Some(old)`）。終了・破棄: `view/fade.rs` の `fade_scenes`（完了時に `replaces` または自身を despawn）、`input/gesture.rs`（割り込み時に即完了扱いにして同フレームの `fade_scenes` で片付け）、`view.rs` の `sync_view(Reset)`（全レイヤー 0 `SceneRoot` 一括掃除）。`Fade` 中は `RedrawScene` / `RelayoutKeepView` を走らせず、`replaces` の孤立を防ぐ。 |
| `MainCamera` | `view.rs` の `setup`・`sync_view`・`reanchor`・`follow_side_panel`、`view/camera.rs` の `refit_on_resize`・`animate_camera`、`input/gesture.rs`・`input/settle.rs`。すべて `scale > 0 && is_finite()` を守る。 |
| `Breadcrumb` / `MenuBreadcrumb` | `view.rs` の `sync_view`・`reanchor`・`navigate`、`input/settle.rs` の `switch_focus`・`focus_node`（すべて `show_breadcrumb` 経由）。 |

### 守るべき不変条件

1. **木と正の場所の整合**: `nav.stack` の先頭から末尾まですべて `FileTree::ROOT` につながっている（`tree.is_attached`）。`layouts.anchor` も `ROOT` につながっており、`nav.top()` は常に `layouts` に含まれ、枠の大きさが正で有限である。
2. **パンくずの一致**: 止まったあと、`Breadcrumb` と `MenuBreadcrumb` は常に `nav.stack` の名前列（`" / "` 区切り）と一致する。
3. **レイヤー 0 の `SceneRoot` の数**: 止まったあと、`CONTENT_LAYER`（レイヤー 0）の `SceneRoot` は 1 個（親の場面を下に敷いているときは 2 個）、動作中（ズーム中・フェード中）も 1〜2 個に限られる。座標系を替えるとき（`sync_view(Reset)`・`reanchor`）は既存のレイヤー 0 `SceneRoot` をすべて消してから作り直す。
4. **カメラの有限性と範囲**: `MainCamera` の位置と倍率は常に有限かつ倍率 > 0。止まったあとの倍率は開いているものの `fit` 倍率と見合う範囲（`fit / MAX_ZOOM_IN ..= root_fit`）に収まる。
5. **操作中の木の不変性**: `Animating` / `Fade` / `Gesture::active` のあいだは裏の読み直し・LAN イベント・`JumpTo` を保留し、操作完了後に木へ当てて `sync_view` で 1 回そろえる。
6. **サムネの色とレイヤー分離**: `CONTENT_LAYER`（0）に中身、`OVERLAY_LAYER`（1）に重複の印・カタログの札と暗い膜・読み込み中の光を分け、`MainCamera` は両方を、サムネ撮影カメラは `CONTENT_LAYER` のみを映す。撮影・読み込みともに `TextureFormat::Rgba8UnormSrgb`（`view_formats` 不使用）に統一する。

## 本物のフォルダを読む

- 起動すると、木は Root > この端末 > ドライブ（`lisa_core::volumes::list` で「既定では読まない」
  もの以外）だけです。`LISA_DUMMY=1` で起動すると、今までのダミーの木を使い、何も読みません。
- フォルダは 1 つずつ、名前順で上のフォルダから深さ優先で読みます（`lisa_core::listing::read_dir`）。
  あるフォルダの下を全部読み終えてから次のフォルダへ進むので、上から順に中身と大きさが確定します。
  読むのは裏のスレッド 1 本で、たどる順番もそのスレッドが持ちます（子のフォルダを名前順の逆に積む）。
  画面で見ている場所は優先しません。ファイルのハッシュは、入れるときに並列にします。
- 読んだ結果は、読む係が SQLite に書きます（`lisa_core::db`、`device`、`drive`、`entry` の表。
  0.3 秒ごとにまとめて確定）。DB は設定と同じアプリのデータのフォルダの `lisa.db` です。
- 木（メモリ）には、見えるところ（並べてある範囲）だけを出します。読み終えたフォルダでも、並べる
  範囲に入るまでは子を出さず（`Node.loaded` が false）、入ったら DB から 1 段ずつ出します
  （`scan.rs` の `load_visible`、1 フレームに 256 フォルダまで）。読む係が書いたばかりでまだ確定して
  いない結果は、確定するまで UI が手元に持ち、DB の代わりに使います。
- 起動したら、DB に残っているところはそのまま使い、まだ読み終えていないフォルダから、同じ深さ優先の
  順で続きを読みます（`Db::unlisted_folders`、読む係のスレッドで探す）。読み終えたフォルダを読み直す
  （変わったかを見る）のは、まだしません。DB が開けないときと ID の無いドライブは、DB に残さず、
  読んだ結果をそのまま木に足します。
- 読み終えたら、同じ読む係が中身をハッシュします（`lisa_core::hash::hash_drive`、blake3）。DB の木を
  深さ優先・名前順に下からたどり、フォルダごとに、ハッシュの無いファイルを 4 本で並列に読みます。
  子がそろったフォルダは、子の（種類、NFC の名前、ハッシュ）から Merkle のハッシュを作ります。
  クラウドにだけあるファイル、リンク、別のボリュームはハッシュせず、大きさだけを入れます。読めなかった
  フォルダは無いものとして親を作ります。止めても、次はハッシュの無いところから続けます。
- 裏で読むときの決まり（`scan/worker.rs`）:
  - 開いている場面のフォルダが変わったら、その頼み（`Request::Visible`）を読む係に送ります。読む係は
    裏の分をすぐやめ（ハッシュは 1 MiB ごとに合図を見る、途中経過は捨てる）、そのフォルダを 1 段読み直し、
    その下のハッシュの無いものをハッシュします（`hash::hash_folder`）。これは下の決まりにかかりません。
    済んだら裏の分に戻ります（ハッシュはドライブの一番上からやり直すが、済んだところは飛ばす）。
  - 読み直して DB と違っていたら（足された・消えた・種類や大きさや日時が変わった）、DB を直し、変わった
    もののハッシュと、そのフォルダと祖先のハッシュを消します（`Db::save_listing` が変わったかを返す）。
    UI は木のそのフォルダの子を外して出し直し、ハッシュし直すまで「古い」にします。祖先のハッシュは、
    読む係が裏で作り直します。
  - 裏の分は I/O を後回しにします（Mac は `setiopolicy_np` の THROTTLE、Windows は
    `THREAD_MODE_BACKGROUND_BEGIN`、`lisa_core::pace`）。速さの上限（設定、既定 200 MB/秒）を守ります。
  - メニューの「一時停止」、電池で動いているとき（設定、既定は休む）、CPU の混み具合（1 分の load average
    をコアの数で割ったもの）が設定のしきい値（既定 80 %）を超えたときは休みます。2 秒ごとに見直します。
    休んでいる理由は左下のリストに出します。
  - ほかのアプリのディスクの読み書きが設定のしきい値（既定 100 MB/秒）を超えたときも休みます。Mac は
    IOKit の `IOBlockStorageDriver` の統計（読み書きの累計）の増えた分から、自分で読んだ分を引いて出します
    （`pace::disk_bytes`）。
- 監視: DB に残すドライブを `notify` で見ます（Mac は FSEvents、Windows は ReadDirectoryChangesW）。変わった
  ものの親のフォルダを読み直しの知らせ（`Request::Hint`）にし、同じものはまとめて、裏の分の決まりに
  従って読み直します。新しくできたフォルダの中も読みます。知らせは取りこぼすことがあるので、開いた
  ときの読み直しもします。アプリ自身の DB と画像の置き場は見ません。
- 前回終わってから変わったところ（Mac）: 読む係は暇なとき（と起動時）に FSEvents のイベント番号を
  `meta` の表に覚えます。起動したら、その番号から今までの履歴（`lisa_core::fsevents::since`）を受け取り、
  変わったフォルダを読み直しの知らせにします。FSEvents は Data のボリュームの中を `/Users/…` のように
  知らせるので、`/System/Volumes/Data` を前に付けて読むドライブの下のパスにします。
- スナップショット: ハッシュがそろうたびに、ドライブの一番上のハッシュが前と違えば `snapshot` の表に
  残し、前のものにつなぎます。フォルダの中身は `tree` の表に、ハッシュをキーにして残します（同じ中身は
  1 回だけ。前の版で作ったフォルダの分は、裏で子の行から埋める）。`lisa-scan snapshots` で一覧を出せます。
- 状態は、ハッシュができたら「読み込み済み」です。ハッシュしないもの（クラウドのみ、リンク、中に入らない
  フォルダ）は一覧に載った時点で読み込み済み、ハッシュできなかったファイルは「失敗」です。
- 読んでいる間は、画面の左下に読み込み中のリストを出します（`ui/progress.rs`）。読んだフォルダの数、
  まだ読んでいない数、速さ、今読んでいるフォルダ、このあと読むフォルダ 5 つです。読み終えたら 4 秒で
  消します。
- 1 フレームに木に足す結果は 256 件までです。画像は、鍵が変わったものだけを作り直します
  （下の「画像キャッシュ」）。
- 読めたら、winit のイベントループを起こし（止まっている間は 1 秒に 1 回しか描かないため）、木に
  足します。木が変わったら、動きが止まるのを待って、0.3 秒に 1 回まで並べ直し、画像を焼き直します。
- 読めなかったフォルダは「失敗」、空のフォルダは「読み込み済み」です。ファイルはハッシュができるまで
  「未読み込み」、できたら「読み込み済み」、フォルダは子から決まります。別のボリュームとクラウドにだけあるフォルダは
  読みません。シンボリックリンクはファイルとして出します（たどらない）。
- 並べる数（`MAX_LAID_OUT`）と画像の数（`MAX_IMAGES`、基準から 2 段下まで）に上限があり、中身の
  多いフォルダが並んでいても重くなりすぎないようにしています。

## 並べ方

- フォルダの中身は、フォルダとファイルをまとめて名前順に、左上から横へタイルにして並べます
  （`layout.rs`）。スクロールはしません。全部をフォルダの本体の中に収めます。
- 列の数は、1 列から中身の数まで試して、タイルの幅がいちばん大きくなるものを選びます。ほぼ同じ
  大きさ（1 % 以内）なら、余るマスが少ないほうにします（`choose_grid`）。タイルの幅は、並べる場所の
  幅の半分まで（中身が少なくても入れ子に見えるように）。並べたタイル全体は真ん中に置きます。
- タイルは 1 つの枠です。枠の中を、本体（フォルダの中身かファイルのプレビュー）と、耳（ファイルは
  名前）で分け合います。フォルダは上に耳、ファイルは下に名前です。枠の大きさは、開いたときの形
  （本体と、開いたときの耳の高さ）で決めます。
- 一覧の中など、開いたときより耳（名前）が大きく描かれるときは、その分だけ本体が縮んで譲ります
  （`render/ear.rs` の `Slot::yielded`）。本体は開いたときの形で描き、入れ物（`YieldBody`）の変形で
  縮めます。枠は変わらないので、フォルダとファイルが並んでいても上下がそろい、隣へ動いても
  ずれません。一覧の中の耳の高さは、枠の高さの 40 % まで。

## 基準のフォルダ（座標の精度）

- 木は深いので、全部を 1 つの座標で並べると f32 の精度が尽きます（1 段で数分の 1 になり、10 段
  くらいで潰れる）。そこで、1 つのフォルダ（基準、`Layouts::anchor`）の本体を、原点を中心にした
  決まった大きさ（`layout.rs` の `anchor_body`）に置き、その周りだけを並べます。
- 並べるのは、基準から Root までの祖先とその子（隣）と、基準の祖父母から下で基準より
  `DEPTH_BELOW` 段下までのものです。並べていないものを引くと、大きさ 0 の枠が返ります。
- 並べ方は相似（大きさに比例するだけ）なので、子の位置から親の位置を逆に求めます。祖先は Root から
  並べ直さず、こうして求めた位置を使います（大きな座標から小さな差を取ると精度が尽きるため）。
- 開いている場面のフォルダが基準と違うまま止まったら（ズーム、フェード、ジェスチャー、画像を
  焼いている途中ではなく、親の場面も下に敷いていないとき）、そのフォルダを基準にして並べ直します
  （`view.rs` の `reanchor`）。カメラは同じ見え方のまま新しい座標に移し、場面を作り直して、
  画像を焼き直します。

## 枠の形

- フォルダの本体（耳を除いた四角）は A4 の横長、√2:1（`layout.rs` の `BODY_ASPECT`）、ファイルの
  プレビューの紙は 4:3（`PREVIEW_ASPECT`）で、高さは同じです。`NodeLayout.frame` は開いたときの
  この四角、`NodeLayout.cell` はタイルの枠（クリックの判定と、開いたときに収める四角）です。
- フォルダの本体の上には、名前の入った耳（タブ）が乗ります（`render/ear.rs`）。横幅は本体の
  1/3 が最小で、名前が長ければ伸びます。
- ファイルの名前はプレビューの下に書きます。大きさは、同じフォルダの中のフォルダの耳の名前と
  同じで、ズームに合わせて一緒に変わります（`draw_name`）。
- 耳の高さは画面上のピクセル数で 2 つ決めます（設定の「耳の高さ」）。「一覧の中」は、親を開いた
  ときの高さで、タイルが小さければタイルの高さの 40 % までに抑えます（`GridLayout.ear`）。開いた
  状態から親の一覧の大きさまで縮めるあいだは、画面上の高さを「開いたとき」から「一覧の中」へ、
  倍率の対数に沿って変えます。それより拡大すれば本体と一緒に大きくなり、それより縮めれば本体と
  一緒に小さくなります。
- 本物の場面の耳と名前は `LiveEar`、それに譲って縮む本体は `YieldBody` で、毎フレーム
  `update_ears` が大きさを合わせます。キャッシュの画像には本体だけを焼き、中の子の耳と名前は
  「一覧の中」の大きさで焼きます。
- 開いたときに画面に収める四角（`NodeLayout.fit`）は、タイルの枠そのものです。フォルダなら上端が
  耳の上端、ファイルなら上端がプレビューの上端、下端が名前の下端になります。これを、メニューバーの
  下の領域に少し余白（`FIT_MARGIN`）を残して収め、真ん中に置きます（`view/camera.rs` の `fit`）。
  余白には外側のフォルダの色が見えます。
- 耳の高さの設定は、本体を画面に収めたときの大きさに対して見ます。

## 読み込みの状態

ファイルとフォルダは、それぞれ読み込みの状態（`model/tree.rs` の `LoadState`）を持ちます。
読み込み済みと言えるのは、ハッシュとフォルダの画像キャッシュが両方そろったときです。中身が
変われば、ハッシュと画像キャッシュは同じタイミングで作り直します。本物の木では、ハッシュができたファイルとフォルダ（およびハッシュしないクラウドのみ・リンク・中に入らないフォルダ）は「読み込み済み」、まだハッシュしていないものは「未読み込み」、読み直して中身が変わったものはハッシュし直すまで「古い」、読めなかったものは「失敗」です。ダミーの木では名前でいくつかの状態に散らしています（`dummy_state`）。

| 状態 | 見た目（`render/status.rs`） |
| --- | --- |
| 未読み込み | 枠だけを薄く描く |
| 読み込み中（待ちも含む） | 青い光が脈打つ。フォルダは読めた分の中身の上に重ねる。画像に焼くときは光を描かない |
| 読み込み済み | そのまま |
| 古い（読み込んだあとに中身が変わった） | 暗い膜をかぶせる |
| 失敗（権限がない、途中で消えた、ディスクのエラーなど） | 赤みのある本体に × |

フォルダの状態は子から決めます（`derive_states`）。中に読み込み中か未読み込みがあれば読み込み中
（全部が未読み込みなら未読み込み）、次に古いものがあれば古いです。子の失敗はフォルダの失敗に
しません。フォルダが失敗になるのは、そのフォルダの一覧が読めなかったときだけです。端末や
ドライブがつながっていないことは、アイテムの状態ではなく端末やドライブの状態として別に扱います。

## 場面と描画の決まり

- 画面に本物として描くのは、今開いているフォルダの「場面」ひとつだけです。中の子フォルダは、
  キャッシュした画像を貼ります。
- 場面には、同じ階層の隣（親の中のほかのタイル）と、外側のフォルダの枠（色だけ）も下に描きます。
  開いたものの周りに隣が見切れて見え、ズームの前後で枠の外の色が変わらないようにするためです。
  隣の 8 つは、動かしている間にちらつかないよう、視錐台カリングしません。
- **重なった場面は、z の大小ではなく作った順に描かれます（後に作ったほうが上）。**
  Bevy 0.20 のスプライトのまとめ描きで、z の並べ替えが効かない場合があったためです。
  上に見せたい場面は、必ず後から作ってください。例:
  - 縮めて親の場面を下に敷いたら、今の場面をその後に作り直す。
  - Esc で戻るときは、親の場面を作ってから、去る場面を作り直してフェードアウトさせる。
- 文字はワールド座標で大きさが決まっていて、カメラと一緒に拡大縮小します。くっきり見える
  ように、その場面を画面いっぱいに見るときの大きさでラスタライズします。

## ズームの動き

始めと終わりの見え方は、ある 1 点を中心に拡大縮小した関係になっています。その点を中心に
倍率だけを変えるので、画面上のどの点もまっすぐ動きます。緩急は倍率の補間（イージング）だけで
つけます（`view/camera.rs` の `zoom_position`）。

## 重複の一覧

- メニューバーの「重複」で、画面の右に一覧を出します（`ui/dups.rs`）。表示だけで、ファイルは動かしません。
- 一覧は、初めて開いたときに裏のスレッドで DB から作ります（`lisa_core::dups::groups`、数十秒）。
  中身がまるごと同じフォルダ（フォルダのハッシュが同じ）は 1 組にし、その中のものは個別に出しません。
  ファイルは同じハッシュで組にし、ハードリンク（同じドライブで同じファイル ID）は同じ実体として 1 つに
  数えます。大きさ 0、クラウドにだけあるもの、ハッシュの無いものは除きます。並びは空く大きさ
  （大きさ ×（数 − 1））の大きい順で、上位 300 組です。`lisa-scan dups` でも同じ一覧を出せます。
- 組を押すと選び、そのコピーが木にあれば暖かい黄色の太い枠と薄い色、まだ木に出ていなければ、出ている
  一番深い祖先と、コピーを含むフォルダに細い枠を付けます（`dups.rs`、`render/scene.rs` の
  `mark_duplicate`。本物の場面にだけ描き、画像には焼かない）。
- 場所を押すと、DB からそこまでの道のりを木に出し（`scan.rs` の `resolve_jump`）、その場所を含む
  フォルダを基準に並べ直して、カメラをそこへ合わせます（`view.rs` の `jump`、ズームの動きはしない）。
- 一覧は右からすべり出し（0.22 秒）、その幅の分だけ木を映す領域を狭めます。木のカメラの viewport を
  狭め、収める計算（`view::view_width`）もその幅で行い、今のものを収め直します。UI は別のカメラで
  ウィンドウ全体に描きます。一覧の上では木を操作しません。

## 画像キャッシュ

- 先読みで焼く仕組みは持たず、**フォルダを開いて本物の場面が描かれたタイミング**で、そのフォルダの本体の範囲（`layouts[folder].frame`）だけを 1024×724 px の画像として 1 枚撮り、ディスクとメモリに保存します（`render/cache.rs` の `update_thumbnails`）。
- 本物の場面は `CONTENT_LAYER`（レイヤー 0: 枠・中身・耳・プレビュー）と `OVERLAY_LAYER`（レイヤー 1: 重複の印・カタログの札と暗い膜・読み込み中の光）に分けてあり、`MainCamera` は両方を、サムネ撮影用の一時カメラは `CONTENT_LAYER` のみを映します。そのため、重複の印・カタログの印・パルス・UI やツールチップはサムネに入りません。
- もう一度同じフォルダを開き直したら、その時点の見た目で撮り直して上書きします。まだ一度も開いていないフォルダは、ディスクに前回の画像があれば裏スレッドで読んで貼り、無ければ画像なし（空の枠）で描きます。サムネ撮影・読み込みは `sync_view` や `reanchor` を一切ブロックしません。
- フォルダごとの鍵（`cache::Key`、blake3）は、描き方の版 `DRAW_VERSION`（現在 `2`）・画像幅と、手元のフォルダならディスク上のパス、LAN の相手のフォルダなら端末 ID ＋端末以下の名前の道のり（その他は Root からの名前の道のり）から作ります。`FolderImages` も `NodeId` ではなく `Key` で持つため、読み直し（`reset_children`）でノード番号が変わっても画像が消えません。中身が変わったフォルダも、開き直すまでは前回のサムネのまま見せます。
- 撮った画像は GPU から読み戻し（`ReadbackOnce`）、裏のスレッドで WebP（非可逆、品質 85）にして `~/Library/Caches/Lisa/folder-images/<鍵>.webp` に残します。撮影用の描画先（`target_image`）も、読み戻し・ディスク読込から作るテクスチャ（`loaded_image`）も同じ `TextureFormat::Rgba8UnormSrgb`（`view_formats` 不使用、OpenGL 安全）にそろえているため、撮影直後と再読込後で色が一致します。
- 画像を読むたびにファイルの更新日時を付け直し、起動時に裏スレッドで 30 日使っていないものと全部で 1 GiB を超える分の古いものから消します（`cache::sweep`）。
- 半透明の色を画像に撮ると色が変わるので、枠や紙の色は「下の色に重ねた結果」を不透明な色として計算して描いています（`render/shapes.rs` の `over`）。

## ファイルの実物のプレビュー（画像）

- カートリッジは、`can_load` と `load`（裏のスレッドで呼ぶ）で、ファイルの中身から小さな RGBA を作れます。今は画像（jpg・jpeg・png・gif・webp）だけで、`image` クレートで読み、長辺 256 px に縮めます。200 MB を超えるファイルと heic は読まず、読めるまで・読めなかったときは今までの山の絵のままです（読めなかったらログに warn を 1 行）。jpg の EXIF の回転はまだ見ていません（後回し）。
- 先読みはしません。`preview::update_previews` が、**今開いているフォルダの直接の子**（手元のファイルだけ。LAN の相手のファイル、焼いた画像の中、まだ開いていないフォルダの中は対象外）のうち、まだ読んでいないものを裏のスレッド 2 本に頼みます。フォルダを開き直す（開いているフォルダや子の数が変わる）たびに、子の鍵を作り直し、大きさか更新日時が変わっていれば読み直します。
- 読んだ絵は `PreviewRegistry` の `PreviewImages` にメモリだけで持ち、ディスクには残しません。鍵はファイルのパス・大きさ・更新日時で、500 枚を超えたら古いものから捨てます。読めた絵が何枚届いても、場面の描き直し（`SyncRequest::request_redraw`）は 1 フレームに 1 回だけ頼みます。
- 描くときは、`DrawContext.loaded`（絵と縦横）があれば、紙の内側に縦横比を保って収めて中央に置きます。

## LAN の他の PC の Lisa を見る（`lan.rs`）

- 設定の「LAN に公開」（`lan_share`）が 1 のあいだ、アプリの中で `lisa_mcp::HttpListener`（既定 `0.0.0.0:47800`、使えなければ空いている番号）と UDP の返事（`0.0.0.0:47801`）を動かします。あわせて、同じフォルダの `lisa-mcp` を別プロセス（sidecar、既定 `0.0.0.0:47802`、`sidecar.json` に `pid` と `port` を記録）として裏で立ち上げます。0 に戻せば `unblock` でアプリの待ち受けを止め、sidecar も `/health` を確かめてから止めます。
- sidecar（`lisa-mcp`）はアプリ本体が落ちたあとも既定で 10 分間（`--linger 600`）残るので、ペアリングした端末から `lisa_logs` で `logs/lisa.log` や `crash.log` を読んだり、`lisa_update` や `lisa_app_start` / `lisa_app_stop` / `lisa_app_alive` でバイナリの入れ替えや再起動を行えます。
- 認証は `lan.json`（`{"token", "peers": [{"device_id", "name", "key", "added_at"}]}`）で、合言葉（`token`）かペアリングで発行した `peer.key` のどちらでも通します（`lisa_update` や `lisa_app_start` / `lisa_app_stop` は `peer.key` のみ）。設定のポップアップの「つないだ端末」から相手ごとに `key` を取り消せます。
- どの側も起動時と 30 秒ごとに UDP 47801 へ `{"lisa":"hello","v":1}` をブロードキャストし、`{"lisa":"here","v":1,"device_id","name","port","mcp_port"}` の返事を 2 秒集めます。
- 見つけた端末と、`peers.json` に覚えている端末を、Root の下に端末ノード（`Node.remote: Some(RemoteRef)`、`path: None`）として並べます。
- 相手の端末ノードが開かれたとき、未ペアリングなら 4 桁の番号を作って `POST /pair` を送り、相手の画面のポップアップで許可されたら返ってきた `key` を `peers.json` に覚えます。つながったら `lisa_drives` でドライブを、フォルダが見えるところに来たら `lisa_list`（1000 件ずつ）で子を裏のスレッドで取って木に足します。木で見るときは、相手の DB を読むだけです。

## 描画頻度

- 止まっている間は 1 秒に 1 回だけ描きます（`WinitSettings` の reactive）。入力があれば、
  すぐ描きます。
- ズーム、ばね、フェード、ジェスチャーの最中だけ、毎フレーム `RequestRedraw` を送ります。
  垂直同期でディスプレイのリフレッシュレートになります。
- デバッグビルドでは、タイトルの FPS とフレーム数で確かめられます（F3）。
- **macOS でのパイプライン描画の無効化と隠れ時の制御**:
  - macOS では `PipelinedRenderingPlugin` を外してメインスレッドで描画します（`main.rs`）。
    `wgpu-hal` 30.0.2 の Metal バックエンドが `acquire_texture` 内で `NSWindow.occlusionState`
    を参照しており、パイプライン描画が有効だとメインスレッド外の Render thread から AppKit の
    `NSWindow` に触れる未定義動作（可視状態でも `Occluded` と誤認される等）になり得るためです。
  - ウィンドウが最小化・他の窓の裏・別スペース等で完全に隠れている（`WindowOccluded(true)`）
    あいだは、macOS の `wgpu-hal` が `SurfaceError::Occluded` を返して `present()` が飛ばされ、
    垂直同期が効かなくなります。そのため隠れている間はサムネ撮影や `pulse_loading`・`keep_redrawing`・
    重複計算の `RequestRedraw` を止めて CPU 暴走を防ぎ、再び見えたとき（`WindowOccluded(false)` /
    `WindowFocused(true)`）に 1 回描き直します。
  - 同じく隠れている間はスワップチェーンが取れず Bevy 0.20 の `Screenshot::primary_window()` が
    全画素 `(0,0,0)` の画像を保存してしまうため、`Command::Capture`（`remote.rs`）は理由付きの
    エラーを返します。
  - 環境変数 `LISA_RENDER_DIAG=1` を付けると、デバッグ・リリースを問わず 1 秒ごとにカメラや
    スワップチェーンの状態をログへ出します（`render/diag.rs`）。

## 今後

- **ジーニースケール（未実装）**: 中身が多いフォルダでは、タイルが小さくなりすぎて見分けにくい
  （500 ファイルの「スキャン」など）。カーソル（指）の近くのタイルだけを大きく見せ、離れるほど
  元の大きさに戻す見せ方（macOS の Dock の拡大のようなもの）を入れる。並べ方（`layout.rs`）は
  変えず、描くときの位置と大きさだけを変える形にすると、クリックの判定や画像キャッシュとの
  食い違いを、拡大している間の当たり判定の直しだけで済ませられる見込み。
