# 保存するデータ（一部実装）

> `device`、`drive`、`entry` の一部は実装しました（`crates/lisa-core/src/db.rs`）。ほかはまだです。
> 考え方は [design.md](design.md) を見てください。

実装した `entry` は、この表のうち `status` 以外です（ハッシュは `hash`、`algo`、`hashed_size`、
`hashed_mtime`。状態はハッシュとエラーから決めます）。`tree` と `snapshot` も実装しました（`tree` の
`entries` は `hash::tree_bytes` の形で、フォルダのハッシュはその blake3）。`hash_queue` も実装済みです。`content` はまだです。加えて、親の中での並び（`pos`）、子を読み終えたか
（`listed`）、ハードリンクの数（`nlink`）、別のボリュームか（`other_volume`）を持ちます。名前は
ディスク上のバイト列（BLOB）です。フォルダの大きさは残さず、読み出すときに子から足します。
端末の ID は `meta` の表（`this_device`）に覚えます。

Lisa が SQLite に残すものと、残さないものをまとめます。

## 考え方

- **中身と場所を分けます。** 中身（ハッシュ）は `content` に 1 行、どこにあるかは `entry` に
  場所ごとに 1 行です。同じファイルが 10 か所にあっても、中身の情報は 1 行で済み、重複は
  `entry.hash` でまとめるだけで見つかります。
- **全部の行が、どの端末・どのドライブのものかを持ちます。** 最初は PC ごとに自分の DB を持ち、
  あとで合わせます。合わせるときに行の持ち主が分かるようにするためです。
- **ID は Lisa が作る UUID か、OS がくれる変わらない ID にします。** ホスト名やドライブ名、
  ドライブレター、マウントしたパスは変わるので、ID にはしません。
- **ハッシュには必ずアルゴリズムを添えます**（今は `blake3`）。あとで変えても混ざらないように。
- 日時は UTC の Unix 時刻（ミリ秒）で持ちます。

## device（端末）

| 列 | 中身 |
|---|---|
| `id` | Lisa が初めて起動したときに作る UUID |
| `name` | 表示名。ユーザーが変えられる。最初はホスト名 |
| `hostname` | OS のホスト名（見分けの手がかり。ID ではない） |
| `os` | `macos` / `windows` と版 |
| `last_seen_at` | 最後にこの端末で Lisa が動いた日時 |

## drive（ドライブ・ボリューム）

| 列 | 中身 |
|---|---|
| `id` | ボリューム UUID。つなぎ直しても、別の PC につないでも同じ |
| `label` | ボリュームの名前（表示用） |
| `fs` | ファイルシステム（APFS、NTFS、exFAT など） |
| `capacity` | 容量（バイト） |
| `case_sensitive` | 大文字小文字を区別するか。同じ名前かどうかの判定が変わる |
| `last_device_id` | 最後につながっていた端末 |
| `last_mount_path` | 最後につながっていた場所（`/Volumes/…`、`D:\` など） |
| `last_seen_at` | 最後に見えた日時 |

## content（中身。ハッシュごとに 1 行）

| 列 | 中身 |
|---|---|
| `hash` | ハッシュ（32 バイト） |
| `algo` | アルゴリズム（`blake3`） |
| `size` | 大きさ（バイト） |
| `first_seen_at` | 初めて見た日時 |

## entry（フォルダ・ファイルの場所）

| 列 | 中身 |
|---|---|
| `id` | 行の番号 |
| `drive_id` | どのドライブか |
| `parent_id` | 親のフォルダ。ドライブの一番上は空 |
| `name` | ディスク上の名前そのまま |
| `name_nfc` | NFC に正規化した名前（並べ替えとフォルダのハッシュに使う） |
| `kind` | `file` / `dir` / `symlink` |
| `size` | 大きさ。フォルダは中の合計 |
| `mtime` | 更新日時 |
| `file_id` | OS のファイル ID（inode、NTFS のファイル ID）。移動の見分けに使う |
| `hash` / `algo` | 中身のハッシュ。フォルダは Merkle ハッシュ |
| `hashed_size` / `hashed_mtime` | ハッシュしたときの大きさと更新日時。今と違えば「古い」 |
| `status` | 未読込 / 読込中 / 読込済 / 古い / 失敗 |
| `error` | 失敗したときの理由 |
| `cloud_only` | クラウドにだけあるファイル（下を見てください） |

- **シンボリックリンク**はたどりません。リンク先の文字列をハッシュします。
- **クラウドにだけあるファイル**（iCloud、OneDrive などのプレースホルダ）は、読むとダウンロードが
  始まるので、ハッシュしません。`cloud_only` を立てて、大きさと日時だけ記録します。
- **フォルダの「読込済」は、ハッシュと画像キャッシュの両方ができたとき**です（下を見てください）。

## tree（フォルダの中身。Git の tree と同じ）

スナップショットの差分を出すには、昔のフォルダの中身が要ります。フォルダのハッシュをキーにして、
その時の子の一覧を残します。中身が同じフォルダは 1 回しか保存されません。

| 列 | 中身 |
|---|---|
| `hash` / `algo` | フォルダのハッシュ |
| `entries` | 子の一覧（種類, NFC の名前, ハッシュ）を名前順に。フォルダのハッシュの元と同じ並び |

## folder_image（フォルダの画像キャッシュ）

フォルダの中身を焼いた画像を、アプリを終わっても残します。

- 画像は DB には入れず、キャッシュのフォルダに PNG などのファイルで置きます。DB には在りかだけを
  持ちます。
- キーはフォルダのハッシュです。中身（子の名前やハッシュ）が変わればハッシュが変わるので、
  画像も新しく焼くことになります。ハッシュを計算し直すのと同じタイミングで焼き直せます。
- ただし画像は、中身のほかに見た目の設定（耳の高さ、端末ごとの色、画像の大きさ、描き方の版）
  でも変わります。これらをまとめた `look` もキーに含めます。設定を変えたら焼き直しです。
- どの `entry` からも指されなくなった画像は、あとで消します。

| 列 | 中身 |
|---|---|
| `hash` / `algo` | フォルダのハッシュ |
| `look` | 見た目の設定をまとめたハッシュ |
| `path` | 画像ファイルの在りか（キャッシュのフォルダから見た相対パス） |
| `width` / `height` | 画像の大きさ（ピクセル） |
| `made_at` | 焼いた日時 |
| `last_used_at` | 最後に使った日時（消すときの目安） |

## hash_queue（ハッシュ待ちの列）

| 列 | 中身 |
|---|---|
| `entry_id` | 主キーと `entry(id)` への外部キー。対象を削除すると連鎖削除 |
| `drive_id` | 取得を接続中のドライブに絞るためのドライブ ID |
| `priority` | 10 は表示中、20 は変更、30 は通常走査と移行 |
| `reason` | `visible`、`changed`、`scheduled`、`retry` |
| `due_at` | 次に試せる UTC Unix ミリ秒。0 は即時 |
| `delay_ms` | 今の待機間隔（ミリ秒） |
| `attempts` | 続けて一時失敗した回数 |
| `last_error` | 直近の一時失敗 |
| `updated_at` | 最終更新の UTC Unix ミリ秒 |

- `hash_queue_pick` はドライブと期限、`hash_queue_due` は期限に使います。`hash_queue_priority` はドライブ・優先度ごとに `due_at, entry_id` 順の先頭を読むために使います。`hash_queue_missing` は再照合待ちのファイルに、`entry_hash_pending` は未完了の子があるかの判定に使います。
- 一覧と同じトランザクションで、ハッシュが必要になった実体ファイルと既読フォルダを登録します。表示中や内容変更で再登録したときは待機状態をリセットし、通常走査では既存の待機状態を維持します。成功・永続失敗では同じトランザクションで列から削除します。
- DB を開くときは表と索引を追加します。旧 DB の補完は接続中のドライブごとに裏で分割実行し、完了を `meta.hash_queue_seeded_v1:<drive_id>` に記録します。

## snapshot（スナップショット）

ドライブごとに撮ります。ドライブは PC の間を移るので、全体で 1 つにするより分かりやすいためです。

| 列 | 中身 |
|---|---|
| `id` | 行の番号 |
| `drive_id` | どのドライブか |
| `root_hash` / `algo` | ドライブの一番上のフォルダのハッシュ |
| `prev_id` | 前のスナップショット（鎖のようにつなぐ） |
| `taken_at` | 撮った日時 |
| `device_id` | どの端末につないで撮ったか |

## catalog_match・content_check・assignment（カタログ）

カタログの当たり、中身の条件の結果、1 か所の仕分けです。列は [catalog.md](catalog.md) の
「保存するデータ（DB）」にあります。

## catalog_dirty（カタログの当て直しが要るフォルダ）

`id INTEGER PRIMARY KEY` だけの表です。`entry` の 3 つのトリガー（`catalog_dirty_insert`・`catalog_dirty_delete`・
`catalog_dirty_update`）が、行の追加・削除と、`parent_id`・`name_nfc`・`kind`・`listed`・`size`・`mtime`・
`cloud_only`・`other_volume` の変更のたびに、親（と、フォルダならそれ自身）の `id` を同じトランザクションで書きます。
アプリのカタログの係が初めて当てるときに作り、読んだ分を消します。行の数はフォルダの数より増えません。

- 古い版の Lisa で開いても、トリガーがこの表に書くだけで動きは変わりません。
- 戻すときは `DROP TRIGGER catalog_dirty_insert; DROP TRIGGER catalog_dirty_delete;
  DROP TRIGGER catalog_dirty_update; DROP TABLE catalog_dirty;`。次に新しい版が起動すると作り直します。

## plan・plan_op（整理の計画）

ファイルの移動・フォルダの作成・隔離フォルダへの退避・取り消しの計画と、1 件ごとの記録です。
詳しくは [organize.md](organize.md) を見てください。

### plan（計画）

| 列 | 中身 |
|---|---|
| `id` | 計画の番号 |
| `state` | `pending` / `approved` / `running` / `done` / `partial` / `rejected` / `cancelled` / `expired` / `undone` |
| `by` | 作った人（`mcp:stdio`・`mcp:http`・`mcp:<相手名>`・`user`・`cli`） |
| `note` | 計画の説明（任意） |
| `summary` | 作ったときの要約（JSON: 件数、動く大きさ、空く大きさ、危ないものの数など） |
| `skip_risky` | 危ないものを除いて許可したか（0 / 1） |
| `created_at` | 作った日時 |
| `decided_at` | 許可・拒否・取り下げ・期限切れが決まった日時 |
| `finished_at` | 実行が終わった日時 |
| `purged_at` | 隔離フォルダから OS のゴミ箱へ送った日時 |

### plan_op（計画の中の 1 件の操作）

| 列 | 中身 |
|---|---|
| `plan_id` / `seq` | 計画の番号と、計画の中の順番（0 始まり） |
| `op` | `mkdir` / `move` / `trash` |
| `drive_id` | 対象のドライブ ID |
| `src` | 元の絶対パス（`mkdir` は作るパス） |
| `dst` | `move` は行き先、`trash` は隔離した場所（実行後）、`mkdir` は新しく作った一番上のフォルダ（すでにあったときは空） |
| `keep` | `trash` で残す方のパス（任意） |
| `size` | 作ったときの大きさ（フォルダは中の合計） |
| `mtime` | 作ったときの更新日時 |
| `hash` | 作ったときのハッシュ（blake3） |
| `risky` | 危ない理由（無ければ空） |
| `state` | `pending` / `running` / `done` / `skipped` / `failed` / `undone` |
| `reason` | `skipped`・`failed` の理由 |
| `done_at` | 終わった日時 |

## 残さないもの

権限、所有者、作成日時、拡張属性、Finder のタグ、ACL。今の使い方（重複を見つけて整理する）には
要らないためです。欲しくなったら足します。
