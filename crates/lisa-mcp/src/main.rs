//! Lisa を MCP（Model Context Protocol）で使うためのサーバー。標準入出力で、1 行 1 つの JSON-RPC を話す。
//!
//! - DB を読む道具（ドライブ、フォルダの中身、名前で探す、重複、同じ中身の場所、スナップショット）は、
//!   アプリが起動していなくても使える。DB は書き換えない。
//! - アプリの画面を動かす道具（`lisa_app_*`）は、起動中のアプリに `lisa_core::remote` で頼む。
//! - ファイルは何も動かさない。
//!
//! ```text
//! cargo build --release -p lisa-mcp
//! claude mcp add lisa -- /path/to/lisa/target/release/lisa-mcp
//! ```
//!
//! `--data-dir <フォルダ>` で、DB と `control.json` の場所を変えられる（試すとき用）。

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use lisa_core::db::Db;
use lisa_core::listing::Kind;
use lisa_core::query::{self, Located};
use lisa_core::remote::{self, Command};
use serde_json::{Value, json};

/// 名前で探す・一覧に出す数の既定と上限。
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
/// 重複の一覧を作るときの組の数（アプリと同じ）。
const MAX_GROUPS: usize = 300;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = match args.iter().position(|a| a == "--data-dir") {
        Some(i) => args.get(i + 1).map(PathBuf::from),
        None => directories::ProjectDirs::from("", "", "Lisa").map(|d| d.data_dir().to_path_buf()),
    };
    let cache_dir =
        directories::ProjectDirs::from("", "", "Lisa").map(|d| d.cache_dir().to_path_buf());
    let Some(data_dir) = data_dir else {
        eprintln!("Lisa のデータの場所が分かりません");
        std::process::exit(1);
    };
    // クラウドのファイルを読んでもダウンロードしない（このプロセスはファイルの中身を読まないが、念のため）。
    let _ = lisa_core::cloud::prevent_downloads();
    let server = Server {
        data_dir,
        cache_dir,
    };
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(&message),
            Err(error) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("JSON が読めません: {error}")},
            })),
        };
        if let Some(response) = response {
            let mut bytes = serde_json::to_vec(&response).expect("JSON にできる");
            bytes.push(b'\n');
            if stdout
                .write_all(&bytes)
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
}

struct Server {
    data_dir: PathBuf,
    cache_dir: Option<PathBuf>,
}

impl Server {
    /// 1 つのメッセージに答える。知らせ（`id` が無い）には答えない。
    fn handle(&self, message: &Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "lisa", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(self.call(&params)),
            _ => Err((-32601, format!("知らないメソッド: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// 道具を呼ぶ。失敗は `isError` の結果で返す（MCP の決まり）。
    fn call(&self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let empty = json!({});
        let args = params.get("arguments").unwrap_or(&empty);
        let result = match name {
            "lisa_drives" => self.drives(),
            "lisa_list" => self.list(args),
            "lisa_info" => self.info(args),
            "lisa_search" => self.search(args),
            "lisa_duplicates" => self.duplicates(args),
            "lisa_copies" => self.copies(args),
            "lisa_snapshots" => self.snapshots(args),
            "lisa_app_state" => self.app(Command::State),
            "lisa_app_show" => self.app_show(args),
            "lisa_app_duplicates" => self.app(Command::Dups {
                open: args.get("open").and_then(Value::as_bool).unwrap_or(true),
                select: args.get("hash").and_then(Value::as_str).map(str::to_string),
            }),
            "lisa_app_rescan" => self.app_rescan(args),
            "lisa_app_pause" => match args.get("paused").and_then(Value::as_bool) {
                Some(paused) => self.app(Command::Pause { paused }),
                None => Err("paused（true か false）が要ります".into()),
            },
            _ => Err(format!("知らない道具: {name}")),
        };
        match result {
            Ok(value) => json!({
                "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
                "structuredContent": value,
            }),
            Err(message) => json!({
                "content": [{"type": "text", "text": message}],
                "isError": true,
            }),
        }
    }

    fn db(&self) -> Result<Db, String> {
        let path = self.data_dir.join("lisa.db");
        if !path.exists() {
            return Err(format!(
                "DB がまだありません（{}）。Lisa を一度起動して読ませてください。",
                path.display()
            ));
        }
        Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn drives(&self) -> Result<Value, String> {
        let db = self.db()?;
        let drives = query::drives(&db).map_err(err)?;
        Ok(json!({"drives": drives}))
    }

    /// `path` か `id` で指したものを DB から探す。
    fn target(&self, db: &Db, args: &Value) -> Result<Located, String> {
        if let Some(id) = args.get("id").and_then(Value::as_i64) {
            return query::path_of(db, id)
                .map_err(err)?
                .ok_or_else(|| format!("id {id} は DB にありません"));
        }
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Err("path か id が要ります".into());
        };
        query::locate(db, Path::new(path))
            .map_err(err)?
            .ok_or_else(|| {
                format!(
                    "{path} は DB にありません（まだ読んでいないか、Lisa が読むドライブの外です）"
                )
            })
    }

    fn list(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let target = self.target(&db, args)?;
        let id = target.chain[target.chain.len() - 1];
        let item = query::item(&db, id).map_err(err)?.ok_or("消えました")?;
        let mut children = query::children(&db, id).map_err(err)?;
        let total = children.len();
        let offset = arg_usize(args, "offset", 0);
        let limit = arg_usize(args, "limit", DEFAULT_LIMIT).min(MAX_LIMIT);
        if args.get("sort").and_then(Value::as_str) == Some("size") {
            children.sort_by_key(|c| std::cmp::Reverse(c.size));
        }
        let shown: Vec<_> = children.into_iter().skip(offset).take(limit).collect();
        Ok(json!({
            "path": target.path,
            "drive_id": target.drive_id,
            "item": item,
            "children_total": total,
            "offset": offset,
            "children": shown,
            "note": "フォルダの size は 0（中の合計は lisa_info の totals で）。",
        }))
    }

    fn info(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let target = self.target(&db, args)?;
        let id = target.chain[target.chain.len() - 1];
        let item = query::item(&db, id).map_err(err)?.ok_or("消えました")?;
        let mut value = json!({
            "path": target.path,
            "drive_id": target.drive_id,
            "chain": target.chain,
            "item": item,
        });
        if item.kind == "dir" && args.get("totals").and_then(Value::as_bool).unwrap_or(false) {
            let (files, dirs, bytes) = query::folder_total(&db, id).map_err(err)?;
            value["totals"] = json!({"files": files, "folders": dirs, "bytes": bytes});
        }
        if let Some(hash) = item.hash.as_deref().and_then(query::parse_hash) {
            let same = query::with_hash(&db, &hash, MAX_LIMIT).map_err(err)?;
            value["same_content_count"] = json!(same.len());
        }
        Ok(value)
    }

    fn search(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let Some(text) = args.get("name").and_then(Value::as_str) else {
            return Err("name が要ります".into());
        };
        let kind = match args.get("kind").and_then(Value::as_str) {
            Some("file") => Some(Kind::File),
            Some("dir") => Some(Kind::Dir),
            _ => None,
        };
        let limit = arg_usize(args, "limit", DEFAULT_LIMIT).min(MAX_LIMIT);
        let ids = query::search_name(&db, text, kind, limit).map_err(err)?;
        let mut found = Vec::new();
        for id in ids {
            if let (Some(item), Some(at)) = (
                query::item(&db, id).map_err(err)?,
                query::path_of(&db, id).map_err(err)?,
            ) {
                found.push(json!({"path": at.path, "drive_id": at.drive_id, "item": item}));
            }
        }
        Ok(json!({"count": found.len(), "limit": limit, "found": found}))
    }

    /// 重複の一覧。アプリのキャッシュが今の DB と合えばそれを、合わなければ作る（数十秒かかる）。
    fn duplicates(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let signature = lisa_core::dups::signature(&db).ok().flatten();
        let cached: Option<lisa_core::dups::Cached> = self
            .cache_dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join("duplicates.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok());
        let (groups, from_cache) = match cached {
            Some(c) if signature.is_some() && signature.as_ref() == Some(&c.signature) => {
                (c.groups, true)
            }
            _ => (
                lisa_core::dups::groups(&db, MAX_GROUPS).map_err(err)?,
                false,
            ),
        };
        // `under` は、そのままと DB の側のパス（firmlink を直したもの）の両方で比べる。
        let unders: Vec<PathBuf> = match args.get("under").and_then(Value::as_str) {
            Some(u) => {
                let mut list = vec![PathBuf::from(u)];
                if let Ok(Some(at)) = query::locate(&db, Path::new(u)) {
                    list.push(at.path);
                }
                list
            }
            None => Vec::new(),
        };
        let min_size = args.get("min_size").and_then(Value::as_u64).unwrap_or(0);
        let kind = args.get("kind").and_then(Value::as_str);
        let limit = arg_usize(args, "limit", 30).min(MAX_GROUPS);
        let matches = |g: &lisa_core::dups::Group| {
            g.size >= min_size
                && kind.is_none_or(|k| query::kind_str(g.kind) == k)
                && (unders.is_empty()
                    || g.copies
                        .iter()
                        .any(|c| unders.iter().any(|u| c.path.starts_with(u))))
        };
        let picked: Vec<Value> = groups
            .iter()
            .filter(|g| matches(g))
            .take(limit)
            .map(|g| {
                json!({
                    "kind": query::kind_str(g.kind),
                    "name": g.name,
                    "size": g.size,
                    "wasted": g.wasted(),
                    "hash": query::hex(&g.hash),
                    "copies": g.copies.iter().map(|c| json!({
                        "path": c.path,
                        "drive_id": c.drive_id,
                        "hard_links": c.hard_links,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let wasted: u64 = groups.iter().map(lisa_core::dups::Group::wasted).sum();
        Ok(json!({
            "groups_total": groups.len(),
            "wasted_total": wasted,
            "from_cache": from_cache,
            "shown": picked.len(),
            "groups": picked,
            "note": "空く大きさ（wasted）の大きい順。中身がまるごと同じフォルダは 1 組にまとめ、その中は個別に出さない。ハードリンクは数えない。",
        }))
    }

    /// 同じ中身のもの（path か hash で）。
    fn copies(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let hash = match args.get("hash").and_then(Value::as_str) {
            Some(text) => query::parse_hash(text).ok_or("hash は 64 文字の 16 進で")?,
            None => {
                let target = self.target(&db, args)?;
                let id = target.chain[target.chain.len() - 1];
                let item = query::item(&db, id).map_err(err)?.ok_or("消えました")?;
                item.hash
                    .as_deref()
                    .and_then(query::parse_hash)
                    .ok_or("まだハッシュしていません")?
            }
        };
        let ids = query::with_hash(&db, &hash, MAX_LIMIT).map_err(err)?;
        let mut copies = Vec::new();
        for id in ids {
            if let (Some(item), Some(at)) = (
                query::item(&db, id).map_err(err)?,
                query::path_of(&db, id).map_err(err)?,
            ) {
                copies.push(json!({"path": at.path, "drive_id": at.drive_id, "item": item}));
            }
        }
        Ok(json!({"hash": query::hex(&hash), "count": copies.len(), "copies": copies}))
    }

    fn snapshots(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let drives = query::drives(&db).map_err(err)?;
        let only = args.get("drive_id").and_then(Value::as_str);
        let mut out = Vec::new();
        for drive in drives.iter().filter(|d| only.is_none_or(|o| o == d.id)) {
            let list = db.snapshots(&drive.id).map_err(err)?;
            out.push(json!({
                "drive_id": drive.id,
                "label": drive.label,
                "snapshots": list.iter().map(|s| json!({
                    "id": s.id,
                    "root_hash": query::hex(&s.root_hash),
                    "prev_id": s.prev_id,
                    "taken_at": s.taken_at,
                })).collect::<Vec<_>>(),
            }));
        }
        Ok(json!({"drives": out}))
    }

    fn app(&self, command: Command) -> Result<Value, String> {
        let reply = remote::send(&self.data_dir, command)?;
        if !reply.ok {
            return Err(reply
                .error
                .unwrap_or_else(|| "うまくいきませんでした".into()));
        }
        serde_json::to_value(reply.state).map_err(err)
    }

    fn app_show(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let target = self.target(&db, args)?;
        let mut state = self.app(Command::Show {
            chain: target.chain.clone(),
        })?;
        state["shown"] = json!(target.path);
        Ok(state)
    }

    fn app_rescan(&self, args: &Value) -> Result<Value, String> {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Err("path が要ります".into());
        };
        // DB の側のパス（firmlink を直したもの）があればそちらで頼む。
        let path = self
            .db()
            .ok()
            .and_then(|db| query::locate(&db, Path::new(path)).ok().flatten())
            .map_or_else(|| PathBuf::from(path), |l| l.path);
        self.app(Command::Rescan { path })
    }
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map_or(default, |v| v as usize)
}

const INSTRUCTIONS: &str = "Lisa は、複数の PC やドライブのファイルを読んでハッシュし、重複を見つけるファイラーです。\
DB を読む道具（lisa_drives, lisa_list, lisa_info, lisa_search, lisa_duplicates, lisa_copies, lisa_snapshots）は\
アプリが起動していなくても使えます。lisa_app_* は起動中のアプリの画面を動かします。どの道具もファイルは動かしません。\
大きさはバイト、日時は UTC の Unix 時刻（ミリ秒）、ハッシュは BLAKE3 の 16 進です。";

/// 道具の一覧（名前、説明、引数の形）。
fn tools() -> Value {
    let target = json!({
        "path": {"type": "string", "description": "ファイルかフォルダの絶対パス（Mac は /Users/… のままでよい）"},
        "id": {"type": "integer", "description": "DB の entry.id（ほかの道具の結果の item.id）。path の代わりに"},
    });
    let with = |extra: Value| {
        let mut props = target.clone();
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        json!({"type": "object", "properties": props})
    };
    let read_only = json!({"readOnlyHint": true, "openWorldHint": false});
    let app_hint = json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false});
    json!([
        {
            "name": "lisa_drives",
            "description": "Lisa が読んだドライブの一覧（ID、名前、最後につないでいた場所、一番上のハッシュ）。",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": read_only,
        },
        {
            "name": "lisa_list",
            "description": "フォルダの中身を DB から出す（1 段だけ）。各子の大きさ、更新日時、ハッシュ、読み終えたか。",
            "inputSchema": with(json!({
                "limit": {"type": "integer", "description": "出す数（既定 100、最大 1000）"},
                "offset": {"type": "integer", "description": "飛ばす数"},
                "sort": {"type": "string", "enum": ["name", "size"], "description": "並び（既定は名前順）"},
            })),
            "annotations": read_only,
        },
        {
            "name": "lisa_info",
            "description": "ファイルかフォルダ 1 つの情報（ハッシュ、同じ中身の数）。totals: true でフォルダの中の合計（大きなフォルダは数秒）。",
            "inputSchema": with(json!({"totals": {"type": "boolean"}})),
            "annotations": read_only,
        },
        {
            "name": "lisa_search",
            "description": "名前に文字列を含むファイルやフォルダを探す（大文字小文字を区別しない）。全部の行を見るので数秒かかる。",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"},
                "kind": {"type": "string", "enum": ["file", "dir"]},
                "limit": {"type": "integer", "description": "既定 100、最大 1000"},
            }, "required": ["name"]},
            "annotations": read_only,
        },
        {
            "name": "lisa_duplicates",
            "description": "重複の一覧（空く大きさの大きい順）。アプリのキャッシュが新しければすぐ、古ければ作り直す（数十秒）。",
            "inputSchema": {"type": "object", "properties": {
                "limit": {"type": "integer", "description": "出す組の数（既定 30、最大 300）"},
                "min_size": {"type": "integer", "description": "1 つあたりの大きさの下限（バイト）"},
                "kind": {"type": "string", "enum": ["file", "dir"]},
                "under": {"type": "string", "description": "このフォルダの下にコピーがある組だけ"},
            }},
            "annotations": read_only,
        },
        {
            "name": "lisa_copies",
            "description": "同じ中身のファイルやフォルダの場所を全部出す（path か id か hash で）。",
            "inputSchema": with(json!({"hash": {"type": "string", "description": "BLAKE3 の 16 進（64 文字）"}})),
            "annotations": read_only,
        },
        {
            "name": "lisa_snapshots",
            "description": "ドライブごとのスナップショット（一番上のハッシュの履歴、新しい順）。",
            "inputSchema": {"type": "object", "properties": {"drive_id": {"type": "string"}}},
            "annotations": read_only,
        },
        {
            "name": "lisa_app_state",
            "description": "起動中のアプリの様子（開いているフォルダ、重複の一覧、読み込みとハッシュの進み具合、一時停止）。",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": read_only,
        },
        {
            "name": "lisa_app_show",
            "description": "起動中のアプリの画面で、その場所へ飛ぶ（ファイルなら、それを含むフォルダを開く）。",
            "inputSchema": with(json!({})),
            "annotations": app_hint,
        },
        {
            "name": "lisa_app_duplicates",
            "description": "起動中のアプリの重複の一覧を開け閉めする。hash を渡すとその組を選び、木の上で光らせる（一覧ができていれば）。",
            "inputSchema": {"type": "object", "properties": {
                "open": {"type": "boolean", "description": "既定 true"},
                "hash": {"type": "string", "description": "選ぶ組のハッシュ（lisa_duplicates の hash）"},
            }},
            "annotations": app_hint,
        },
        {
            "name": "lisa_app_rescan",
            "description": "起動中のアプリに、そのフォルダを先に読み直して（ハッシュして）もらう。",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
            "annotations": app_hint,
        },
        {
            "name": "lisa_app_pause",
            "description": "起動中のアプリの裏の読み込みを一時停止する（paused: true）・再開する（false）。",
            "inputSchema": {"type": "object", "properties": {"paused": {"type": "boolean"}}, "required": ["paused"]},
            "annotations": app_hint,
        },
    ])
}
