//! Lisa を MCP（Model Context Protocol）で使うためのサーバーの中身。
//!
//! - DB を読む道具（ドライブ、フォルダの中身、名前で探す、重複、同じ中身の場所、スナップショット、
//!   カタログの一覧・当たり・要約・テスト）は、アプリが起動していなくても使える。書き込むのは
//!   カタログの TOML ファイル（`lisa_catalog_save`）と DB の `assignment` の表（`lisa_assign` /
//!   `lisa_unassign`）だけ。
//! - アプリの画面を動かす道具（`lisa_app_*`）は、起動中のアプリに `lisa_core::remote` で頼む。
//! - ユーザーのファイルを動かすには `lisa_plan_create` で計画を出し、人が Lisa の画面（か CLI）で
//!   許可したときだけ実行される（許可する道具は作らない。取り消しは `lisa_undo`）。ペアリングした端末から
//!   sidecar に頼む `lisa_update` だけ、`lisa` / `lisa-mcp` の実行ファイルを入れ替える。

mod catalog;
mod keep_relay;
pub use keep_relay::{link_relay_client, relay_client, relocation_client};
mod organize;
pub use organize::NO_PLAN_FOR_REQUEST;
mod remote_keep;
#[cfg(feature = "verify-fs")]
mod verify_fs;

#[cfg(all(windows, feature = "verify-fs"))]
pub use verify_fs::{isolated_directory, validate_isolated_tree};

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use lisa_core::db::Db;
use lisa_core::listing::Kind;
use lisa_core::query::{self, Located};
use lisa_core::remote::{self, Command};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, StatusCode};

/// 名前で探す・一覧に出す数の既定と上限。
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
/// 重複の一覧を作るときの組の数（アプリと同じ）。
const MAX_GROUPS: usize = 300;
/// `lisa_logs` で出す行数の既定と上限。
const DEFAULT_LOG_LINES: usize = 200;
const MAX_LOG_LINES: usize = 2000;

/// `--listen` の既定の待ち受け先。
pub const DEFAULT_LISTEN: &str = "0.0.0.0:47800";
/// アプリが別プロセスで立ち上げる `lisa-mcp`（sidecar）の既定のポート番号。
pub const SIDECAR_PORT: u16 = 47802;
/// アプリが別プロセスで立ち上げた `lisa-mcp` が自分の pid と port を書くファイルの名前。
pub const SIDECAR_FILE: &str = "sidecar.json";
/// `--linger` の既定の秒数（親が消えてから残る時間）。
pub const DEFAULT_LINGER_SECS: u64 = 600;
/// LAN で互いを見つける UDP の番号。
pub const DISCOVERY_PORT: u16 = 47801;
/// HTTP で受ける本文の上限（1 MiB）。
const MAX_BODY: usize = 1024 * 1024;
/// 端末間の付け替えで、4 MiB の小分けを base64 で運ぶ要求（中継と、認証済みの `/mcp`）の上限。
const MAX_TRANSFER_BODY: usize = 6 * 1024 * 1024;
/// LAN 用の合言葉とペアリングした相手を覚えておくファイルの名前。
pub const LAN_FILE: &str = "lan.json";

/// `sidecar.json` の中身（立ち上がっている sidecar の pid と port）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarInfo {
    pub pid: u32,
    pub port: u16,
}

/// `lan.json` の中身（合言葉と、ペアリングを許可した相手の一覧）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LanConfig {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

/// ペアリングを許可した相手（`lan.json` の `peers` の 1 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Peer {
    pub device_id: String,
    pub name: String,
    pub key: String,
    pub added_at: i64,
}

/// `POST /pair` で届くペアリングの頼み。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairRequest {
    pub device_id: String,
    pub name: String,
    pub code: String,
}

/// `POST /pair` が許可されたときに返す返事。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairReply {
    pub key: String,
    pub device_id: String,
    pub name: String,
}

/// ペアリングの頼みを受けて、許可なら `Some(PairReply)`、断る・時間切れなら `None` を返す関数。
pub type PairHandler = Box<dyn Fn(PairRequest) -> Option<PairReply> + Send + Sync>;

/// 頼んできた相手が誰として通ったか。
/// `lisa_update` や `lisa_app_start` / `lisa_app_stop` は `Peer` のときだけ使える。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Auth {
    /// 標準入出力。
    #[default]
    Stdio,
    /// 合言葉（`lan.json` の `token` か `--token`）。
    Token,
    /// ペアリングした端末の `peer.key`。
    Peer {
        device_id: String,
        name: String,
        remote_ip: Option<IpAddr>,
    },
}

/// `lisa_update` で落とすファイルの上限（512 MiB）。
const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;
/// `lisa_logs` でファイル末尾から読むバイト数の上限（2 MiB）。
const MAX_LOG_READ_BYTES: u64 = 2 * 1024 * 1024;
/// HTTP で同時にさばく要求の上限。
const MAX_ACTIVE_REQUESTS: usize = 32;
/// `POST /pair` で同時に返事を待つ数の上限。
const MAX_ACTIVE_PAIRS: usize = 4;
/// ペアリングや `here` パケットで受ける `name`・`device_id` の最大文字数。
const MAX_PEER_FIELD_CHARS: usize = 64;
/// このプロセス（sidecar など）が覚えておくアプリの pid（`--parent` か `lisa_app_start` で起動したもの）。
static KNOWN_APP_PID: AtomicU32 = AtomicU32::new(0);
/// `--data-dir` がコマンドラインで明示されたか。
static CUSTOM_DATA_DIR: AtomicBool = AtomicBool::new(false);
/// `lisa_update` の同時実行を防ぐロック。
static UPDATE_LOCK: Mutex<()> = Mutex::new(());

/// 覚えているアプリの pid を設定する（`--parent` やテスト用）。
pub fn set_known_app_pid(pid: u32) {
    KNOWN_APP_PID.store(pid, Ordering::Relaxed);
}

/// 覚えているアプリの pid を読む（無ければ None）。
pub fn known_app_pid() -> Option<u32> {
    let pid = KNOWN_APP_PID.load(Ordering::Relaxed);
    (pid != 0).then_some(pid)
}

/// `--data-dir` が明示されたことを記録する（子プロセスに `LISA_DATA_DIR` を引き継ぐかの判定用）。
pub fn set_custom_data_dir(custom: bool) {
    CUSTOM_DATA_DIR.store(custom, Ordering::Relaxed);
}

pub struct Server {
    pub data_dir: PathBuf,
    pub cache_dir: Option<PathBuf>,
}

impl Server {
    /// 1 つのメッセージに答える（標準入出力の認証として扱う）。知らせ（`id` が無い）には答えない。
    pub fn handle(&self, message: &Value) -> Option<Value> {
        self.handle_with_auth(message, &Auth::Stdio)
    }

    /// 認証の結果 `auth` を添えて 1 つのメッセージに答える。知らせ（`id` が無い）には答えない。
    pub fn handle_with_auth(&self, message: &Value, auth: &Auth) -> Option<Value> {
        self.handle_in_role(message, auth, true)
    }

    /// HTTP の口から届いた 1 つのメッセージに答える。`sidecar` が `false`（アプリ本体の 47800）のときは
    /// `lisa_update` / `lisa_app_start` / `lisa_app_stop` / `lisa_screenshot` を断る。
    pub fn handle_in_role(&self, message: &Value, auth: &Auth, sidecar: bool) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let device = self.device_label();
                let name = match &device {
                    Some(d) => format!("lisa ({d})"),
                    None => "lisa".to_string(),
                };
                let instructions = match &device {
                    Some(d) => format!("このサーバーは {d} の Lisa です。{INSTRUCTIONS}"),
                    None => INSTRUCTIONS.to_string(),
                };
                Ok(json!({
                    "protocolVersion": params
                        .get("protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": name,
                        "version": env!("CARGO_PKG_VERSION"),
                        "os": std::env::consts::OS,
                    },
                    "instructions": instructions,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(self.call_in_role(&params, auth, sidecar)),
            _ => Err((-32601, format!("知らないメソッド: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// この PC の Lisa の表示名（DB の `meta.this_device` と `device` 表から）。取れなければ None。
    pub fn device_label(&self) -> Option<String> {
        let db = self.db().ok()?;
        let (name, hostname) = query::this_device(&db).ok()??;
        let name = name.trim();
        let hostname = hostname.trim();
        match (!name.is_empty(), !hostname.is_empty()) {
            (true, true) if name != hostname => Some(format!("{name} / {hostname}")),
            (true, _) => Some(name.to_string()),
            (false, true) => Some(hostname.to_string()),
            (false, false) => None,
        }
    }

    /// 道具を呼ぶ。失敗は `isError` の結果で返す（MCP の決まり）。
    pub fn call(&self, params: &Value, auth: &Auth) -> Value {
        self.call_in_role(params, auth, true)
    }

    fn call_in_role(&self, params: &Value, auth: &Auth, sidecar: bool) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let empty = json!({});
        let args = params.get("arguments").unwrap_or(&empty);
        let result = match name {
            "lisa_drives" => self.drives(),
            "lisa_list" => self.list(args),
            "lisa_info" => self.info(args),
            "lisa_merged_resolve" => require_peer(auth).and_then(|_| {
                let marker: lisa_core::merged::Marker =
                    serde_json::from_value(args["marker"].clone()).map_err(err)?;
                Ok(
                    match lisa_core::merged::resolve_target(&self.db()?, &marker) {
                        Ok(chain) => json!({ "chain": chain }),
                        Err(error) => json!({ "error": error }),
                    },
                )
            }),
            "lisa_search" => self.search(args),
            "lisa_duplicates" => self.duplicates(args),
            "lisa_duplicate_catalog" | "lisa_duplicate_locations" | "lisa_duplicate_groups" => {
                require_peer(auth).and_then(|_| self.duplicate_comparison(name, args, auth))
            }
            "lisa_keep_inspect" | "lisa_keep_reserve" | "lisa_keep_renew" | "lisa_keep_release" => {
                self.signed_duplicate_keep(name, args, auth)
            }
            "lisa_merged_link_update" | "lisa_merged_receive_transfer" => {
                self.signed_duplicate_keep(name, args, auth)
            }
            "lisa_merged_link_permits" | "lisa_merged_link_revoke" => {
                self.link_relay_permit(name, args, auth)
            }
            "lisa_merged_delete_choices" => self.db().and_then(|db| {
                let target = self.target(&db, args)?;
                let sources = lisa_core::organize::linked_delete_sources(&db, &target.path)?;
                Ok(json!({ "sources": sources }))
            }),
            "lisa_merged_link_status" => require_peer(auth).and_then(|_| {
                let id = args["link_id"].as_str().ok_or("リンク ID がありません")?;
                let status = lisa_core::merged::links::status(&self.db()?, id)?;
                Ok(json!({ "status": status }))
            }),
            "lisa_merged_link_sources" => require_peer(auth).and_then(|_| {
                let db = self.db()?;
                let entry = args["entry_id"]
                    .as_i64()
                    .ok_or("実体の項目 ID がありません")?;
                let links = lisa_core::merged::links::sources_for_entry(&db, entry)?;
                Ok(json!({ "sources": links, "checked_at": lisa_core::db::now_ms() }))
            }),
            "lisa_keep_relay_issue" | "lisa_keep_relay_revoke" => {
                self.keep_relay_grant(name, args, auth)
            }
            "lisa_copies" => self.copies(args),
            "lisa_snapshots" => self.snapshots(args),
            "lisa_logs" => self.logs(args),
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
            "lisa_update" => require_peer(auth)
                .and_then(|ip| {
                    require_sidecar(sidecar, name)?;
                    Ok(ip)
                })
                .and_then(|ip| self.update(args, ip)),
            "lisa_app_start" => require_peer(auth)
                .and_then(|_| require_sidecar(sidecar, name))
                .and_then(|_| self.app_start(args)),
            "lisa_app_stop" => require_peer(auth)
                .and_then(|_| require_sidecar(sidecar, name))
                .and_then(|_| self.app_stop(args)),
            "lisa_screenshot" => require_peer(auth)
                .and_then(|_| require_sidecar(sidecar, name))
                .and_then(|_| self.screenshot(args)),
            #[cfg(feature = "verify-fs")]
            "lisa_verify_fs" => require_peer(auth)
                .and_then(|_| require_sidecar(sidecar, name))
                .and_then(|_| verify_fs::call(&self.data_dir, args)),
            #[cfg(feature = "verify-fs")]
            "lisa_verify_control" => require_peer(auth)
                .and_then(|_| require_sidecar(sidecar, name))
                .and_then(|_| {
                    let command: Command =
                        serde_json::from_value(args["command"].clone()).map_err(err)?;
                    #[cfg(windows)]
                    if let Some(path) = args.get("data_dir") {
                        let path = path.as_str().ok_or("data_dir は文字列で指定してください")?;
                        let dir = isolated_directory(path)?;
                        let mut reply =
                            serde_json::to_value(remote::send(&dir, command)?).map_err(err)?;
                        if args["include_log"] == true {
                            reply["log"] = read_log_tail(&dir.join("logs").join("lisa.log"), 100);
                        }
                        return Ok(reply);
                    }
                    let capture = match &command {
                        Command::Capture { name } => Some(name.clone()),
                        _ => None,
                    };
                    let mut reply = serde_json::to_value(remote::send(&self.data_dir, command)?)
                        .map_err(err)?;
                    // アプリ自身が撮った画像（描画の読み戻し）を、OS の画面取得と見比べるために返す。
                    if let Some(name) = capture {
                        let path = self
                            .data_dir
                            .join("screenshots")
                            .join(format!("{name}.png"));
                        for _ in 0..50 {
                            if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 500) {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        reply["png_base64"] =
                            json!(base64_encode(&std::fs::read(&path).map_err(err)?));
                    }
                    Ok(reply)
                }),
            "lisa_app_alive" => self.app_alive(),
            "lisa_catalogs" => self.catalogs(args),
            "lisa_catalog_matches" => self.catalog_matches(args),
            "lisa_unclassified" => self.unclassified(args),
            "lisa_outline" => self.outline(args),
            "lisa_catalog_test" => self.catalog_test(args),
            "lisa_catalog_save" => self.catalog_save(args),
            "lisa_assign" => self.assign(args),
            "lisa_unassign" => self.unassign(args),
            "lisa_plan_create" => self.plan_create(args, auth),
            "lisa_duplicate_sources" => self.duplicate_sources(args, auth),
            "lisa_duplicate_local_plan" => self.duplicate_local_plan(args, auth),
            "lisa_duplicate_plan_create" => self.duplicate_plan_create(args, auth),
            "lisa_rename" => self.rename(args, auth),
            "lisa_plan_status" => self.plan_status(args, auth),
            "lisa_plans" => self.plans(args),
            "lisa_plan_cancel" => self.plan_cancel(args),
            "lisa_plan_request_undo" => self.request_undo(args, auth),
            "lisa_undo" => self.undo(args),
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

    /// LAN の重複パネル専用の読み取り操作。
    fn duplicate_comparison(&self, name: &str, args: &Value, auth: &Auth) -> Result<Value, String> {
        use lisa_core::dups;
        // 比較索引の写しは依頼元ごとに最新の 1 つだけ持つ。
        let owner = match auth {
            Auth::Peer { device_id, .. } => device_id.as_str(),
            Auth::Token | Auth::Stdio => "",
        };
        let db = self.db()?;
        let generation = args.get("generation").and_then(Value::as_str);
        match name {
            "lisa_duplicate_catalog" => serde_json::to_value(dups::catalog_page(
                &db,
                generation,
                arg_usize(args, "offset", 0),
                owner,
            )?)
            .map_err(err),
            "lisa_duplicate_locations" => {
                let generation = generation.ok_or("索引の世代が要ります")?;
                let keys = serde_json::from_value::<Vec<dups::ContentKey>>(args["keys"].clone())
                    .map_err(err)?;
                serde_json::to_value(dups::locations(&db, generation, &keys)?).map_err(err)
            }
            _ => serde_json::to_value(dups::device_groups(&db, MAX_GROUPS)?).map_err(err),
        }
    }

    /// 重複の一覧。アプリのキャッシュが今の DB と合えばそれを、合わなければ作る（数十秒かかる）。
    fn duplicates(&self, args: &Value) -> Result<Value, String> {
        let db = self.db()?;
        let signature = lisa_core::dups::signature(&db).ok().flatten();
        let cached: Option<lisa_core::dups::Cached> = self
            .cache_dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join(lisa_core::dups::CACHE_FILE)).ok())
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

    fn logs(&self, args: &Value) -> Result<Value, String> {
        let lines = arg_usize(args, "lines", DEFAULT_LOG_LINES).min(MAX_LOG_LINES);
        let logs_dir = self.data_dir.join("logs");
        Ok(json!({
            "data_dir": self.data_dir,
            "lines": lines,
            "lisa_log": read_log_tail(&logs_dir.join("lisa.log"), lines),
            "lisa_1_log": read_log_tail(&logs_dir.join("lisa.1.log"), lines),
            "crash_log": read_log_tail(&self.data_dir.join("crash.log"), lines),
        }))
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

    fn update(&self, args: &Value, remote_ip: Option<IpAddr>) -> Result<Value, String> {
        let Some(target) = args.get("target").and_then(Value::as_str) else {
            return Err("target（\"lisa\" か \"lisa-mcp\"）が要ります".into());
        };
        if target != "lisa" && target != "lisa-mcp" {
            return Err(format!(
                "target は \"lisa\" か \"lisa-mcp\" だけです: {target}"
            ));
        }
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return Err("url が要ります".into());
        };
        let Some(blake3_hex) = args.get("blake3").and_then(Value::as_str) else {
            return Err("blake3（64 文字の 16 進）が要ります".into());
        };
        let restart = args.get("restart").and_then(Value::as_bool).unwrap_or(true);
        let dir = current_exe_dir()?;
        let mut stop_info: Option<Value> = None;
        let outcome = update_binary_in_with(&dir, target, url, blake3_hex, remote_ip, || {
            if target == "lisa" && restart {
                let stop_val = self
                    .app_stop(&json!({}))
                    .map_err(|e| format!("入れ替えの前にアプリを止められませんでした: {e}"))?;
                if stop_val.get("alive").and_then(Value::as_bool) == Some(true) {
                    return Err(
                        "入れ替えの前にアプリを止められませんでした（まだ動いています）".into(),
                    );
                }
                stop_info = Some(stop_val);
            }
            Ok(())
        })?;
        let mut out = json!({
            "target": target,
            "path": outcome.path,
            "old_path": outcome.old_path,
            "old_size": outcome.old_size,
            "old_blake3": outcome.old_blake3,
            "new_size": outcome.new_size,
            "new_blake3": outcome.new_blake3,
            "restart": restart,
        });
        if target == "lisa" && restart {
            if let Some(stop_val) = stop_info {
                out["stop"] = stop_val;
            }
            let start_res = self.app_start_in(&dir, &[], false);
            out["start"] = match start_res {
                Ok(v) => v,
                Err(e) => json!({"error": e}),
            };
        } else if target == "lisa-mcp" && restart {
            out["restarting_self"] = json!(true);
        }
        Ok(out)
    }

    fn app_start(&self, args: &Value) -> Result<Value, String> {
        #[cfg(all(windows, feature = "verify-fs"))]
        if let Some(env) = args.get("env").and_then(Value::as_object)
            && (env.contains_key("LISA_DATA_DIR") || env.contains_key("LISA_VERIFY_SCAN_ROOT"))
        {
            if env.len() != 2 {
                return Err("検証起動にはデータと走査ルートの 2 項目を指定してください".into());
            }
            let data = isolated_directory(
                env["LISA_DATA_DIR"]
                    .as_str()
                    .ok_or("LISA_DATA_DIR が必要です")?,
            )?;
            let root = isolated_directory(
                env["LISA_VERIFY_SCAN_ROOT"]
                    .as_str()
                    .ok_or("LISA_VERIFY_SCAN_ROOT が必要です")?,
            )?;
            verify_fs::validate_isolated_tree(&data)?;
            verify_fs::validate_isolated_tree(&root)?;
            let folded_data = PathBuf::from(data.to_string_lossy().to_ascii_lowercase());
            let folded_root = PathBuf::from(root.to_string_lossy().to_ascii_lowercase());
            if folded_data.starts_with(&folded_root) || folded_root.starts_with(&folded_data) {
                return Err("検証データと走査ルートは別ディレクトリにしてください".into());
            }
            if args.get("restart").and_then(Value::as_bool) == Some(true)
                || self.app_alive()?["process_alive"] == true
            {
                return Err("検証起動の前に Lisa を終了してください".into());
            }
            let exe = current_exe_dir()?.join(bin_file_name("lisa"));
            let env = [
                ("LISA_DATA_DIR".into(), data.to_string_lossy().into_owned()),
                (
                    "LISA_VERIFY_SCAN_ROOT".into(),
                    root.to_string_lossy().into_owned(),
                ),
                ("LISA_DEBUG_REMOTE".into(), "1".into()),
                ("LISA_DEBUG_LAN_DISCOVERY".into(), "0".into()),
            ];
            let pid = spawn_detached_app(&exe, &data, &env)?;
            set_known_app_pid(pid);
            thread::sleep(Duration::from_secs(5));
            return Ok(json!({
                "pid": pid,
                "alive": is_process_alive(pid),
                "responding": remote::send(&data, Command::State).is_ok_and(|r| r.ok),
                "log": read_log_tail(&data.join("logs").join("lisa.log"), 100),
            }));
        }
        let env = parse_app_start_env(args)?;
        let restart = args
            .get("restart")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let dir = current_exe_dir()?;
        self.app_start_in(&dir, &env, restart)
    }

    /// `exe_dir` の `lisa`（Windows は `lisa.exe`）を起動する。すでに `control.json` に返事があれば何もしない
    /// （`restart: true` なら先に止めてから起動する）。
    pub fn app_start_in(
        &self,
        exe_dir: &Path,
        extra_env: &[(String, String)],
        restart: bool,
    ) -> Result<Value, String> {
        let mut stop_info: Option<Value> = None;
        if restart {
            let stop_val = self
                .app_stop(&json!({}))
                .map_err(|e| format!("再起動の前にアプリを止められませんでした: {e}"))?;
            if stop_val.get("alive").and_then(Value::as_bool) == Some(true) {
                return Err("再起動の前にアプリを止められませんでした（まだ動いています）".into());
            }
            stop_info = Some(stop_val);
        } else if let Ok(reply) = remote::send(&self.data_dir, Command::State)
            && reply.ok
        {
            let pid = remote::read_endpoint(&self.data_dir).map(|e| e.pid);
            if let Some(p) = pid {
                set_known_app_pid(p);
            }
            return Ok(json!({
                "already_running": true,
                "pid": pid,
                "alive": true,
                "responding": true,
            }));
        }
        // 返事の無い古い control.json が残っていれば消してから起動する。
        if remote::read_endpoint(&self.data_dir).is_some_and(|e| !is_lisa_process_alive(e.pid)) {
            let _ = std::fs::remove_file(self.data_dir.join(remote::FILE_NAME));
        }
        let exe = exe_dir.join(bin_file_name("lisa"));
        if !exe.exists() {
            return Err(format!(
                "アプリの実行ファイルがありません: {}",
                exe.display()
            ));
        }
        let pid = spawn_detached_app(&exe, &self.data_dir, extra_env)?;
        set_known_app_pid(pid);
        thread::sleep(Duration::from_secs(5));
        let alive = is_process_alive(pid);
        let responding = remote::send(&self.data_dir, Command::State).is_ok_and(|r| r.ok);
        let env_obj: serde_json::Map<String, Value> = extra_env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        let mut out = json!({
            "already_running": false,
            "started": true,
            "pid": pid,
            "alive": alive,
            "responding": responding,
            "path": exe,
            "env": env_obj,
        });
        if let Some(stop_val) = stop_info {
            out["stop"] = stop_val;
        }
        Ok(out)
    }

    fn app_stop(&self, args: &Value) -> Result<Value, String> {
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let endpoint_pid = remote::read_endpoint(&self.data_dir).map(|e| e.pid);
        let remembered_pid = known_app_pid();
        let endpoint_alive = endpoint_pid.is_some_and(is_lisa_process_alive);
        let remembered_alive = remembered_pid.is_some_and(is_lisa_process_alive);
        let target_pid = endpoint_pid
            .filter(|_| endpoint_alive)
            .or_else(|| remembered_pid.filter(|_| remembered_alive));

        if !force && remote::send(&self.data_dir, Command::Quit).is_ok_and(|r| r.ok) {
            let wait_pid = target_pid.or(endpoint_pid);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                let alive = wait_pid.is_some_and(is_lisa_process_alive);
                if !alive && !self.data_dir.join(remote::FILE_NAME).exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let still_alive = wait_pid.is_some_and(is_lisa_process_alive);
            if !still_alive {
                set_known_app_pid(0);
                let _ = std::fs::remove_file(self.data_dir.join(remote::FILE_NAME));
                return Ok(json!({
                    "stopped": true,
                    "method": "quit",
                    "pid": wait_pid,
                    "alive": false,
                }));
            }
        }

        // Quit が通らず、control.json の pid も Lisa のプロセスとして生きていなければ古いファイルを消す。
        if endpoint_pid.is_some() && !endpoint_alive {
            let _ = std::fs::remove_file(self.data_dir.join(remote::FILE_NAME));
        }
        if remembered_pid.is_some() && !remembered_alive {
            set_known_app_pid(0);
        }

        // force: true、または Quit に返事が無い・5 秒たっても終わらないときは pid を強制終了する。
        let Some(pid) = target_pid else {
            if let Some(stale_pid) = endpoint_pid.or(remembered_pid) {
                return Ok(json!({
                    "stopped": true,
                    "method": "already_exited",
                    "pid": stale_pid,
                    "alive": false,
                }));
            }
            return Ok(json!({
                "stopped": false,
                "method": "none",
                "pid": Value::Null,
                "alive": false,
                "note": "動いているアプリの pid が分かりません（すでに終わっているか、control.json がありません）",
            }));
        };
        if !is_lisa_process_alive(pid) {
            set_known_app_pid(0);
            let _ = std::fs::remove_file(self.data_dir.join(remote::FILE_NAME));
            return Ok(json!({
                "stopped": true,
                "method": "already_exited",
                "pid": pid,
                "alive": false,
            }));
        }
        terminate_process(pid)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && is_lisa_process_alive(pid) {
            thread::sleep(Duration::from_millis(50));
        }
        let alive = is_lisa_process_alive(pid);
        if !alive {
            set_known_app_pid(0);
            let _ = std::fs::remove_file(self.data_dir.join(remote::FILE_NAME));
        }
        Ok(json!({
            "stopped": !alive,
            "method": "terminate",
            "pid": pid,
            "alive": alive,
        }))
    }

    fn app_alive(&self) -> Result<Value, String> {
        let mut control = remote::read_endpoint(&self.data_dir);
        let control_pid = control.as_ref().map(|e| e.pid);
        let remembered_pid = known_app_pid();
        let control_pid_alive = control_pid.is_some_and(is_lisa_process_alive);
        let remembered_pid_alive = remembered_pid.is_some_and(is_lisa_process_alive);
        let responding = remote::send(&self.data_dir, Command::State).is_ok_and(|r| r.ok);
        if control.is_some() && !control_pid_alive && !responding {
            let _ = std::fs::remove_file(self.data_dir.join(remote::FILE_NAME));
            control = None;
        }
        let control_pid = control.as_ref().map(|e| e.pid);
        if !remembered_pid_alive
            && remembered_pid.is_some()
            && (!responding || control_pid != remembered_pid)
        {
            set_known_app_pid(0);
        }
        if responding && let Some(p) = control_pid {
            set_known_app_pid(p);
        } else if control_pid_alive && let Some(p) = control_pid {
            set_known_app_pid(p);
        }
        let pid = if control_pid_alive || (responding && control_pid.is_some()) {
            control_pid
        } else if remembered_pid_alive {
            remembered_pid
        } else {
            control_pid.or(remembered_pid.filter(|_| remembered_pid_alive))
        };
        let process_alive = control_pid_alive || remembered_pid_alive || responding;
        Ok(json!({
            "pid": pid,
            "control_pid": control_pid,
            "known_pid": known_app_pid(),
            "process_alive": process_alive,
            "responding": responding,
            "control_json_exists": control.is_some(),
        }))
    }

    fn screenshot(&self, args: &Value) -> Result<Value, String> {
        let parsed = parse_screenshot_args(args)?;
        let app_pid = remote::read_endpoint(&self.data_dir)
            .map(|e| e.pid)
            .or_else(known_app_pid);
        let mut frames: Vec<RawShot> = Vec::with_capacity(parsed.count);
        let mut total_b64_bytes: usize = 0;
        for i in 0..parsed.count {
            if i > 0 {
                if let Some(last) = frames.last()
                    && total_b64_bytes + base64_encoded_len(last.bmp.len())
                        > MAX_SCREENSHOT_TOTAL_BYTES
                {
                    break;
                }
                thread::sleep(Duration::from_millis(parsed.interval_ms));
            }
            let shot = capture_one_shot(parsed.window, app_pid, parsed.scale)?;
            let b64_len = base64_encoded_len(shot.bmp.len());
            if !frames.is_empty() && total_b64_bytes + b64_len > MAX_SCREENSHOT_TOTAL_BYTES {
                break;
            }
            total_b64_bytes += b64_len;
            frames.push(shot);
        }
        Ok(pack_screenshots(parsed, frames, MAX_SCREENSHOT_TOTAL_BYTES))
    }
}

/// `lisa_screenshot` の返事の合計の上限（40 MB）。
pub const MAX_SCREENSHOT_TOTAL_BYTES: usize = 40 * 1024 * 1024;

/// `lisa_screenshot` の引数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenshotArgs {
    pub count: usize,
    pub interval_ms: u64,
    pub window: bool,
    pub scale: u32,
}

/// `lisa_screenshot` の引数を検査して取り出す。
pub fn parse_screenshot_args(args: &Value) -> Result<ScreenshotArgs, String> {
    let count = match args.get("count") {
        None | Some(Value::Null) => 6,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| "count は 1〜12 の整数で指定してください".to_string())?;
            if !(1..=12).contains(&n) {
                return Err(format!("count は 1〜12 の範囲で指定してください: {n}"));
            }
            n as usize
        }
    };
    let interval_ms = match args.get("interval_ms") {
        None | Some(Value::Null) => 80,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| "interval_ms は 30〜2000 の整数で指定してください".to_string())?;
            if !(30..=2000).contains(&n) {
                return Err(format!(
                    "interval_ms は 30〜2000 の範囲で指定してください: {n}"
                ));
            }
            n as u64
        }
    };
    let window = match args.get("window") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| "window は true か false で指定してください".to_string())?,
    };
    let scale = match args.get("scale") {
        None | Some(Value::Null) => 2,
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| "scale は 1〜4 の整数で指定してください".to_string())?;
            if !(1..=4).contains(&n) {
                return Err(format!("scale は 1〜4 の範囲で指定してください: {n}"));
            }
            n as u32
        }
    };
    Ok(ScreenshotArgs {
        count,
        interval_ms,
        window,
        scale,
    })
}

/// 撮った 1 枚の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawShot {
    pub taken_at: i64,
    pub width: u32,
    pub height: u32,
    pub window: bool,
    pub bmp: Vec<u8>,
}

/// 撮った枚の一覧を `max_total_bytes`（base64 の合計）に収まる範囲で JSON にまとめる。
/// 少なくとも先頭 1 枚は入れ、枚数を減らしたときは `truncated: true` と `note` を添える。
pub fn pack_screenshots(
    args: ScreenshotArgs,
    frames: Vec<RawShot>,
    max_total_bytes: usize,
) -> Value {
    let first_ts = frames.first().map_or(0, |f| f.taken_at);
    let mut shots = Vec::with_capacity(frames.len());
    let mut total_bmp_bytes: usize = 0;
    let mut total_b64_bytes: usize = 0;
    let mut any_window = false;
    let mut width = 0;
    let mut height = 0;
    for (idx, frame) in frames.into_iter().enumerate() {
        let b64 = base64_encode(&frame.bmp);
        if !shots.is_empty() && total_b64_bytes + b64.len() > max_total_bytes {
            break;
        }
        if idx == 0 {
            width = frame.width;
            height = frame.height;
        }
        if frame.window {
            any_window = true;
        }
        let elapsed_ms = (frame.taken_at - first_ts).max(0) as u64;
        total_bmp_bytes += frame.bmp.len();
        total_b64_bytes += b64.len();
        shots.push(json!({
            "index": idx,
            "taken_at": frame.taken_at,
            "elapsed_ms": elapsed_ms,
            "width": frame.width,
            "height": frame.height,
            "window": frame.window,
            "bytes": frame.bmp.len(),
            "bmp_base64": b64,
        }));
    }
    let kept = shots.len();
    let truncated = kept < args.count;
    let mut notes: Vec<String> = Vec::new();
    if truncated {
        notes.push(format!(
            "返事の合計が 40 MB を超えるため、枚数を {} 枚から {} 枚に減らしました。",
            args.count, kept
        ));
    }
    if args.window && !any_window {
        notes.push("Lisa の窓ではなく画面全体を撮りました。".into());
    }
    let mut out = json!({
        "count": kept,
        "requested_count": args.count,
        "interval_ms": args.interval_ms,
        "scale": args.scale,
        "window_requested": args.window,
        "window": any_window,
        "width": width,
        "height": height,
        "total_bytes": total_bmp_bytes,
        "total_base64_bytes": total_b64_bytes,
        "truncated": truncated,
        "shots": shots,
    });
    if !notes.is_empty() {
        out["note"] = json!(notes.join(" "));
    }
    out
}

/// 上から下・左から右に並んだ RGB 画素列（`width * height * 3` バイト）を 24 ビット BMP に書き出す。
pub fn encode_bmp24(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("BMP の幅と高さは 1 以上が要ります".into());
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| "BMP の大きさが大きすぎます".to_string())?;
    if rgb.len() != expected {
        return Err(format!(
            "RGB のバイト数が合いません（期待: {expected}、実際: {}）",
            rgb.len()
        ));
    }
    let row_bytes = (width as usize) * 3;
    let stride = (row_bytes + 3) & !3;
    let padding = stride - row_bytes;
    let image_size = stride
        .checked_mul(height as usize)
        .ok_or_else(|| "BMP の画像部分が大きすぎます".to_string())?;
    let file_size = 54usize
        .checked_add(image_size)
        .ok_or_else(|| "BMP のファイルが大きすぎます".to_string())?;
    let file_size_u32 =
        u32::try_from(file_size).map_err(|_| "BMP が 4 GiB を超えています".to_string())?;
    let image_size_u32 =
        u32::try_from(image_size).map_err(|_| "BMP が 4 GiB を超えています".to_string())?;

    let mut out = Vec::with_capacity(file_size);
    // BITMAPFILEHEADER (14 バイト)
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size_u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER (40 バイト)
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes()); // 正の高さ = 下の行から順
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&image_size_u32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 DPI
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    // 画素データ（下の行から上の行へ、1 画素は B, G, R、各行の終わりは 4 バイト境界まで 0 埋め）
    let pad_bytes = [0u8; 3];
    for y in (0..height as usize).rev() {
        let row = &rgb[y * row_bytes..(y + 1) * row_bytes];
        for px in row.as_chunks::<3>().0 {
            out.push(px[2]);
            out.push(px[1]);
            out.push(px[0]);
        }
        if padding > 0 {
            out.extend_from_slice(&pad_bytes[..padding]);
        }
    }
    Ok(out)
}

/// 24 ビットまたは 32 ビットの BMP バイト列から、上から下・左から右の RGB 画素列を取り出す。
pub fn decode_bmp_to_rgb(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return Err("BMP の見出しがありません".into());
    }
    let bf_off_bits = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
    let bi_size = u32::from_le_bytes(bytes[14..18].try_into().unwrap()) as usize;
    let bi_width = i32::from_le_bytes(bytes[18..22].try_into().unwrap());
    let bi_height = i32::from_le_bytes(bytes[22..26].try_into().unwrap());
    let bi_bit_count = u16::from_le_bytes(bytes[28..30].try_into().unwrap());
    let bi_compression = u32::from_le_bytes(bytes[30..34].try_into().unwrap());
    if bi_size < 40 || bi_width <= 0 || bi_height == 0 {
        return Err("BMP の大きさが不正です".into());
    }
    if bi_bit_count != 24 && bi_bit_count != 32 {
        return Err(format!("対応していない BMP のビット数です: {bi_bit_count}"));
    }
    if bi_compression != 0 && !(bi_bit_count == 32 && bi_compression == 3) {
        return Err(format!(
            "圧縮された BMP には対応していません: {bi_compression}"
        ));
    }
    let width = bi_width as u32;
    let top_down = bi_height < 0;
    let height = bi_height.unsigned_abs();
    let bytes_per_pixel = (bi_bit_count / 8) as usize;
    let row_bytes = (width as usize) * bytes_per_pixel;
    let stride = (row_bytes + 3) & !3;
    let needed = bf_off_bits
        .checked_add(
            stride
                .checked_mul(height as usize)
                .ok_or("BMP が大きすぎます")?,
        )
        .ok_or("BMP が大きすぎます")?;
    if bytes.len() < needed {
        return Err("BMP の画素データが途中で切れています".into());
    }
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for dst_y in 0..height as usize {
        let src_y = if top_down {
            dst_y
        } else {
            (height as usize) - 1 - dst_y
        };
        let row_start = bf_off_bits + src_y * stride;
        let row = &bytes[row_start..row_start + row_bytes];
        for px in row.chunks_exact(bytes_per_pixel) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
    }
    Ok((width, height, rgb))
}

/// 上から下・左から右の RGB 画素列を、縦横 `scale`（1〜4）で割った大きさに縮小する。
pub fn downsample_rgb(
    src_w: u32,
    src_h: u32,
    rgb: &[u8],
    scale: u32,
) -> Result<(u32, u32, Vec<u8>), String> {
    if src_w == 0 || src_h == 0 || !(1..=4).contains(&scale) {
        return Err("縮小の引数が不正です".into());
    }
    let expected = (src_w as usize) * (src_h as usize) * 3;
    if rgb.len() != expected {
        return Err("RGB の長さが合いません".into());
    }
    if scale == 1 {
        return Ok((src_w, src_h, rgb.to_vec()));
    }
    let dst_w = (src_w / scale).max(1);
    let dst_h = (src_h / scale).max(1);
    let mut out = Vec::with_capacity((dst_w as usize) * (dst_h as usize) * 3);
    let src_w_usize = src_w as usize;
    let s = scale as usize;
    for dy in 0..dst_h as usize {
        let y0 = (dy * s).min(src_h as usize - 1);
        let y1 = ((dy + 1) * s).min(src_h as usize).max(y0 + 1);
        for dx in 0..dst_w as usize {
            let x0 = (dx * s).min(src_w_usize - 1);
            let x1 = ((dx + 1) * s).min(src_w_usize).max(x0 + 1);
            let mut r_sum: u32 = 0;
            let mut g_sum: u32 = 0;
            let mut b_sum: u32 = 0;
            let mut count: u32 = 0;
            for sy in y0..y1 {
                let row_off = sy * src_w_usize * 3;
                for sx in x0..x1 {
                    let idx = row_off + sx * 3;
                    r_sum += u32::from(rgb[idx]);
                    g_sum += u32::from(rgb[idx + 1]);
                    b_sum += u32::from(rgb[idx + 2]);
                    count += 1;
                }
            }
            out.push((r_sum / count) as u8);
            out.push((g_sum / count) as u8);
            out.push((b_sum / count) as u8);
        }
    }
    Ok((dst_w, dst_h, out))
}

/// 上から下・左から右の BGRA 画素列（`src_w * src_h * 4` バイト）を、縦横 `scale`（1〜4）で割った RGB に変換する。
pub fn downsample_bgra_to_rgb(
    src_w: u32,
    src_h: u32,
    bgra: &[u8],
    scale: u32,
) -> Result<(u32, u32, Vec<u8>), String> {
    if src_w == 0 || src_h == 0 || !(1..=4).contains(&scale) {
        return Err("縮小の引数が不正です".into());
    }
    let expected = (src_w as usize) * (src_h as usize) * 4;
    if bgra.len() != expected {
        return Err("BGRA の長さが合いません".into());
    }
    let dst_w = (src_w / scale).max(1);
    let dst_h = (src_h / scale).max(1);
    let mut out = Vec::with_capacity((dst_w as usize) * (dst_h as usize) * 3);
    if scale == 1 {
        for px in bgra.as_chunks::<4>().0 {
            out.push(px[2]);
            out.push(px[1]);
            out.push(px[0]);
        }
        return Ok((dst_w, dst_h, out));
    }
    let src_w_usize = src_w as usize;
    let s = scale as usize;
    for dy in 0..dst_h as usize {
        let y0 = (dy * s).min(src_h as usize - 1);
        let y1 = ((dy + 1) * s).min(src_h as usize).max(y0 + 1);
        for dx in 0..dst_w as usize {
            let x0 = (dx * s).min(src_w_usize - 1);
            let x1 = ((dx + 1) * s).min(src_w_usize).max(x0 + 1);
            let mut r_sum: u32 = 0;
            let mut g_sum: u32 = 0;
            let mut b_sum: u32 = 0;
            let mut count: u32 = 0;
            for sy in y0..y1 {
                let row_off = sy * src_w_usize * 4;
                for sx in x0..x1 {
                    let idx = row_off + sx * 4;
                    b_sum += u32::from(bgra[idx]);
                    g_sum += u32::from(bgra[idx + 1]);
                    r_sum += u32::from(bgra[idx + 2]);
                    count += 1;
                }
            }
            out.push((r_sum / count) as u8);
            out.push((g_sum / count) as u8);
            out.push((b_sum / count) as u8);
        }
    }
    Ok((dst_w, dst_h, out))
}

pub use lisa_core::base64::{
    decode as base64_decode, encode as base64_encode, encoded_len as base64_encoded_len,
};

/// 1 枚の画面を撮り、縮小して 24 ビット BMP にする。
fn capture_one_shot(window: bool, app_pid: Option<u32>, scale: u32) -> Result<RawShot, String> {
    #[cfg(windows)]
    {
        capture_one_shot_windows(window, app_pid, scale)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (window, app_pid);
        capture_one_shot_macos(scale)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (window, app_pid, scale);
        Err("この OS では画面を撮れません（Windows と macOS だけ対応しています）".into())
    }
}

#[cfg(target_os = "macos")]
fn capture_one_shot_macos(scale: u32) -> Result<RawShot, String> {
    static SHOT_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path =
        std::env::temp_dir().join(format!("lisa-mcp-shot-{}-{seq}.bmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp_path);
    let taken_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let output = std::process::Command::new("screencapture")
        .args(["-x", "-t", "bmp"])
        .arg(&tmp_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("screencapture を実行できませんでした: {e}"))?;
    if !output.status.success() || !tmp_path.exists() {
        let _ = std::fs::remove_file(&tmp_path);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(format!(
                "screencapture が失敗しました（画面収録の許可が無い可能性があります: {}）",
                output.status
            ));
        }
        return Err(format!("screencapture が失敗しました: {stderr}"));
    }
    let raw_bytes = std::fs::read(&tmp_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("撮った BMP を読めませんでした: {e}")
    })?;
    let _ = std::fs::remove_file(&tmp_path);
    let (src_w, src_h, rgb) = decode_bmp_to_rgb(&raw_bytes)?;
    let (dst_w, dst_h, scaled_rgb) = downsample_rgb(src_w, src_h, &rgb, scale)?;
    let bmp = encode_bmp24(dst_w, dst_h, &scaled_rgb)?;
    Ok(RawShot {
        taken_at,
        width: dst_w,
        height: dst_h,
        window: false,
        bmp,
    })
}

#[cfg(windows)]
fn capture_one_shot_windows(
    window: bool,
    app_pid: Option<u32>,
    scale: u32,
) -> Result<RawShot, String> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
        SRCCOPY, SelectObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetProcessDPIAware,
    };
    use windows_sys::core::BOOL;

    struct FindWindowCtx {
        target_pid: u32,
        best_rect: Option<RECT>,
        best_area: i64,
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: EnumWindows から渡した FindWindowCtx への可変参照として使う。
        let ctx = unsafe { &mut *(lparam as *mut FindWindowCtx) };
        if unsafe { IsWindowVisible(hwnd) } == 0 || unsafe { IsIconic(hwnd) } != 0 {
            return 1;
        }
        let mut win_pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut win_pid) };
        if win_pid != ctx.target_pid {
            return 1;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let dwm_ok = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut rect as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
        } == 0;
        if !dwm_ok && unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            return 1;
        }
        let w = i64::from(rect.right - rect.left);
        let h = i64::from(rect.bottom - rect.top);
        if w > 0 && h > 0 && w * h > ctx.best_area {
            ctx.best_area = w * h;
            ctx.best_rect = Some(rect);
        }
        1
    }

    let (x, y, width, height, captured_window) = unsafe {
        let _ = SetProcessDPIAware();
        let mut vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let mut vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let mut vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let mut vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if vw <= 0 || vh <= 0 {
            vx = 0;
            vy = 0;
            vw = GetSystemMetrics(SM_CXSCREEN);
            vh = GetSystemMetrics(SM_CYSCREEN);
        }
        if vw <= 0 || vh <= 0 {
            return Err("画面の大きさを取れませんでした（GetSystemMetrics）".into());
        }
        let mut win_rect: Option<(i32, i32, i32, i32)> = None;
        if window && let Some(pid) = app_pid.filter(|&p| p != 0) {
            let mut ctx = FindWindowCtx {
                target_pid: pid,
                best_rect: None,
                best_area: 0,
            };
            let _ = EnumWindows(
                Some(enum_windows_proc),
                (&mut ctx as *mut FindWindowCtx) as LPARAM,
            );
            if let Some(r) = ctx.best_rect {
                let left = r.left.max(vx);
                let top = r.top.max(vy);
                let right = r.right.min(vx + vw);
                let bottom = r.bottom.min(vy + vh);
                if right > left && bottom > top {
                    win_rect = Some((left, top, right - left, bottom - top));
                }
            }
        }
        match win_rect {
            Some((wx, wy, ww, wh)) => (wx, wy, ww, wh, true),
            None => (vx, vy, vw, vh, false),
        }
    };

    let taken_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);

    let bgra = unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return Err("画面の DC（GetDC）を取れませんでした".into());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err("メモリ DC（CreateCompatibleDC）を作れませんでした".into());
        }
        let hbm = CreateCompatibleBitmap(screen_dc, width, height);
        if hbm.is_null() {
            DeleteDC(mem_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err("ビットマップ（CreateCompatibleBitmap）を作れませんでした".into());
        }
        let old_obj = SelectObject(mem_dc, hbm);
        let blt_ok = BitBlt(
            mem_dc,
            0,
            0,
            width,
            height,
            screen_dc,
            x,
            y,
            SRCCOPY | CAPTUREBLT,
        ) != 0;
        if !blt_ok {
            let err = std::io::Error::last_os_error();
            SelectObject(mem_dc, old_obj);
            DeleteObject(hbm);
            DeleteDC(mem_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err(format!("BitBlt に失敗しました: {err}"));
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = width;
        bmi.bmiHeader.biHeight = -height; // 上から下の順で受け取る
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
        let lines = GetDIBits(
            mem_dc,
            hbm,
            0,
            height as u32,
            buf.as_mut_ptr().cast(),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem_dc, old_obj);
        DeleteObject(hbm);
        DeleteDC(mem_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);
        if lines == 0 {
            return Err("GetDIBits で画素を取り出せませんでした".into());
        }
        buf
    };

    let (dst_w, dst_h, rgb) = downsample_bgra_to_rgb(width as u32, height as u32, &bgra, scale)?;
    let bmp = encode_bmp24(dst_w, dst_h, &rgb)?;
    Ok(RawShot {
        taken_at,
        width: dst_w,
        height: dst_h,
        window: captured_window,
        bmp,
    })
}

fn require_peer(auth: &Auth) -> Result<Option<IpAddr>, String> {
    match auth {
        Auth::Peer { remote_ip, .. } => Ok(*remote_ip),
        Auth::Token | Auth::Stdio => Err(
            "この道具はペアリングした端末からだけ使えます（合言葉や標準入出力では使えません）"
                .into(),
        ),
    }
}

fn require_sidecar(sidecar: bool, tool_name: &str) -> Result<(), String> {
    if sidecar {
        Ok(())
    } else {
        Err(format!(
            "{tool_name} はアプリの中の口（47800）では使えません。sidecar（47802）に頼んでください"
        ))
    }
}

/// 今の実行ファイルがあるフォルダ。
fn current_exe_dir() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("自分の exe の場所が分かりません: {e}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("親フォルダがありません: {}", exe.display()))
}

/// OS に合わせた実行ファイル名（Windows は `.exe` 付き）。
pub fn bin_file_name(target: &str) -> String {
    if cfg!(windows) {
        format!("{target}.exe")
    } else {
        target.to_string()
    }
}

/// OS に合わせた `.old` 退避ファイル名。
fn old_bin_file_name(target: &str, suffix: Option<u128>) -> String {
    match (cfg!(windows), suffix) {
        (true, None) => format!("{target}.old.exe"),
        (true, Some(ts)) => format!("{target}.old-{ts}.exe"),
        (false, None) => format!("{target}.old"),
        (false, Some(ts)) => format!("{target}.old-{ts}"),
    }
}

/// 使われていない古い `*.old-*.exe` / `*.old-*` ファイルを掃除する（Windows で使用中のものは失敗して残る）。
fn cleanup_unused_old_binaries(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if is_timestamped_old_binary_name(name_str) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn is_timestamped_old_binary_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    for prefix in ["lisa.old-", "lisa-mcp.old-"] {
        if let Some(rest) = stem.strip_prefix(prefix)
            && !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// `update_binary_in` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub old_size: Option<u64>,
    pub old_blake3: Option<String>,
    pub new_size: u64,
    pub new_blake3: String,
}

/// `url`（http のみ。ホストが `expected_ip` と同じであること）から `<target>.new` に落とし、
/// `expected_blake3` と合えば `dir` の実行ファイルと入れ替える。`expected_ip` が `None` なら断る。
pub fn update_binary_in(
    dir: &Path,
    target: &str,
    url: &str,
    expected_blake3: &str,
    expected_ip: Option<IpAddr>,
) -> Result<UpdateOutcome, String> {
    update_binary_in_with(dir, target, url, expected_blake3, expected_ip, || Ok(()))
}

fn update_binary_in_with(
    dir: &Path,
    target: &str,
    url: &str,
    expected_blake3: &str,
    expected_ip: Option<IpAddr>,
    before_replace: impl FnOnce() -> Result<(), String>,
) -> Result<UpdateOutcome, String> {
    if target != "lisa" && target != "lisa-mcp" {
        return Err(format!(
            "target は \"lisa\" か \"lisa-mcp\" だけです: {target}"
        ));
    }
    let expected_hash = query::parse_hash(expected_blake3)
        .ok_or_else(|| "blake3 は 64 文字の 16 進で指定してください".to_string())?;
    let expected_hex = query::hex(&expected_hash);

    let url_ip = parse_http_url_ip(url)?;
    let Some(remote_ip) = expected_ip else {
        return Err("頼んできた相手の IP アドレスが分からないため入れ替えられません".into());
    };
    if normalize_ip(url_ip) != normalize_ip(remote_ip) {
        return Err(format!(
            "url のホスト（{url_ip}）が頼んできた相手の IP（{remote_ip}）と違います"
        ));
    }

    update_binary_locked(dir, target, url, &expected_hex, before_replace)
}

#[cfg(test)]
fn update_binary_for_test(
    dir: &Path,
    target: &str,
    url: &str,
    expected_blake3: &str,
) -> Result<UpdateOutcome, String> {
    let ip = parse_http_url_ip(url)?;
    update_binary_in(dir, target, url, expected_blake3, Some(ip))
}

fn update_binary_locked(
    dir: &Path,
    target: &str,
    url: &str,
    expected_hex: &str,
    before_replace: impl FnOnce() -> Result<(), String>,
) -> Result<UpdateOutcome, String> {
    let _guard = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let new_path = dir.join(format!("{target}.new"));
    let _ = std::fs::remove_file(&new_path);

    let download_res = (|| -> Result<(u64, String), String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_recv_body(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .get(url)
            .call()
            .map_err(|e| format!("{url} から落とせませんでした: {e}"))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!("{url} が HTTP {status} を返しました"));
        }
        let mut reader = response.body_mut().as_reader();
        let mut file =
            open_new_executable(&new_path).map_err(|e| format!("{}: {e}", new_path.display()))?;
        let mut hasher = lisa_core::hash::Hasher::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("読み込みの途中で切れました: {e}"))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > MAX_UPDATE_BYTES {
                return Err(format!(
                    "ファイルが大きすぎます（上限 {} MiB）",
                    MAX_UPDATE_BYTES / (1024 * 1024)
                ));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])
                .map_err(|e| format!("{}: {e}", new_path.display()))?;
        }
        file.flush()
            .map_err(|e| format!("{}: {e}", new_path.display()))?;
        drop(file);
        let actual = query::hex(hasher.finalize().as_bytes());
        if actual != expected_hex {
            return Err(format!(
                "blake3 が合いません（期待: {expected_hex}、実際: {actual}）"
            ));
        }
        Ok((total, actual))
    })();

    let (new_size, new_blake3) = match download_res {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&new_path);
            return Err(e);
        }
    };

    if let Err(e) = before_replace() {
        let _ = std::fs::remove_file(&new_path);
        return Err(e);
    }

    cleanup_unused_old_binaries(dir);

    let exe_path = dir.join(bin_file_name(target));
    let (old_size, old_blake3) = if exe_path.exists() {
        let size = std::fs::metadata(&exe_path).ok().map(|m| m.len());
        let hash = lisa_core::hash::hash_file(&exe_path)
            .ok()
            .map(|h| query::hex(&h));
        (size, hash)
    } else {
        (None, None)
    };

    let old_path = if exe_path.exists() {
        let primary_old = dir.join(old_bin_file_name(target, None));
        let dest_old = if primary_old.exists() && std::fs::remove_file(&primary_old).is_err() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            dir.join(old_bin_file_name(target, Some(ts)))
        } else {
            primary_old
        };
        if let Err(e) = std::fs::rename(&exe_path, &dest_old) {
            let _ = std::fs::remove_file(&new_path);
            return Err(format!(
                "{} を {} に退避できませんでした: {e}",
                exe_path.display(),
                dest_old.display()
            ));
        }
        Some(dest_old)
    } else {
        None
    };

    if let Err(e) = std::fs::rename(&new_path, &exe_path) {
        // 戻せるなら元の exe を戻す。
        if let Some(ref old) = old_path {
            let _ = std::fs::rename(old, &exe_path);
        }
        let _ = std::fs::remove_file(&new_path);
        return Err(format!(
            "{} を {} に置けませんでした: {e}",
            new_path.display(),
            exe_path.display()
        ));
    }

    Ok(UpdateOutcome {
        path: exe_path,
        old_path,
        old_size,
        old_blake3,
        new_size,
        new_blake3,
    })
}

fn open_new_executable(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    options.open(path)
}

/// `http://<host>[:port]/...` から `host` の IP アドレスを取り出す（`http://` 以外やホスト名・不正な形は断る）。
fn parse_http_url_ip(url: &str) -> Result<IpAddr, String> {
    let Some(rest) = url.trim().strip_prefix("http://") else {
        return Err("url は http:// で始まるものだけ使えます".into());
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("url のホストが読めません: {url}"));
    }
    let host = strip_port(authority);
    let host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<IpAddr>()
        .map_err(|_| format!("url のホストは IP アドレスで指定してください: {host}"))
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(v4) => IpAddr::V4(v4),
    }
}

/// 子プロセス（`lisa`）に `LISA_DATA_DIR` を渡すべきか。
/// 自分が `--data-dir` や `LISA_DATA_DIR` で動いているとき（既定の `data_dir` と違うとき）だけ渡す。
fn should_pass_data_dir_env(data_dir: &Path) -> bool {
    if CUSTOM_DATA_DIR.load(Ordering::Relaxed) {
        return true;
    }
    if std::env::var_os("LISA_DATA_DIR").is_some_and(|s| !s.is_empty()) {
        return true;
    }
    let default_dir =
        directories::ProjectDirs::from("", "", "Lisa").map(|d| d.data_dir().to_path_buf());
    default_dir.as_deref() != Some(data_dir)
}

/// `lisa_app_start` で子プロセスに渡してよい環境変数の名前。
pub const ALLOWED_APP_ENV_KEYS: &[&str] = &[
    "WGPU_BACKEND",
    "LISA_PRESENT_MODE",
    "LISA_UPDATE_MODE",
    "RUST_LOG",
    "LISA_RENDER_DIAG",
];
/// 検証用ビルドだけ、アプリの制御口の検証用の頼み（`LISA_DEBUG_REMOTE`）を開けられる。
fn verify_env_key(key: &str) -> bool {
    cfg!(feature = "verify-fs") && matches!(key, "LISA_DEBUG_REMOTE" | "LISA_WINDOW_SIZE")
}
/// `lisa_app_start` で渡す環境変数の値の最大文字数。
const MAX_APP_ENV_VALUE_LEN: usize = 64;

/// `lisa_app_start` の引数 `env`（文字列から文字列の object）を検査して取り出す。
pub fn parse_app_start_env(args: &Value) -> Result<Vec<(String, String)>, String> {
    let Some(raw) = args.get("env") else {
        return Ok(Vec::new());
    };
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let Some(obj) = raw.as_object() else {
        return Err("env は文字列から文字列の object で指定してください".into());
    };
    let mut out = Vec::with_capacity(obj.len());
    for (key, val) in obj {
        let Some(val_str) = val.as_str() else {
            return Err(format!("env.{key} の値は文字列で指定してください"));
        };
        validate_app_env_pair(key, val_str)?;
        out.push((key.clone(), val_str.to_string()));
    }
    Ok(out)
}

/// `lisa_app_start` に渡す環境変数 1 組（キーと値）が許されるものか検査する。
pub fn validate_app_env_pair(key: &str, value: &str) -> Result<(), String> {
    if !ALLOWED_APP_ENV_KEYS.contains(&key) && !verify_env_key(key) {
        return Err(format!(
            "env のキー {key:?} は使えません（使えるのは {} だけです）",
            ALLOWED_APP_ENV_KEYS.join("、")
        ));
    }
    if value.len() > MAX_APP_ENV_VALUE_LEN {
        return Err(format!(
            "env.{key} の値が長すぎます（{MAX_APP_ENV_VALUE_LEN} 文字まで）"
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b',' | b'=' | b':' | b'.' | b'-'))
    {
        return Err(format!(
            "env.{key} の値に使えない文字が入っています（英数字と _,=:.- だけ使えます）"
        ));
    }
    Ok(())
}

/// アプリを別プロセス（Windows は `DETACHED_PROCESS`）で立ち上げ、その pid を返す。
fn spawn_detached_app(
    exe: &Path,
    data_dir: &Path,
    extra_env: &[(String, String)],
) -> Result<u32, String> {
    let mut cmd = std::process::Command::new(exe);
    if should_pass_data_dir_env(data_dir) {
        cmd.env("LISA_DATA_DIR", data_dir);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(parent) = exe.parent() {
        cmd.current_dir(parent);
    }
    #[cfg(windows)]
    let mut child = {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x0000_0008)
        cmd.creation_flags(0x0000_0008);
        cmd.spawn()
            .map_err(|e| format!("{} を起動できませんでした: {e}", exe.display()))?
    };
    #[cfg(not(windows))]
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{} を起動できませんでした: {e}", exe.display()))?;
    let pid = child.id();
    #[cfg(windows)]
    let _ = &mut child;
    #[cfg(not(windows))]
    {
        // Unix では親である自分が生きている間に子が終わるとゾンビ（defunct）として残り、
        // `kill -0` が通り続けてしまうので、別スレッドで回収する。
        let _ = thread::Builder::new()
            .name("lisa-app-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
    Ok(pid)
}

/// 今のアプリの pid（`control.json` か `known_app_pid`）を返す。
pub fn current_app_pid(data_dir: &Path) -> Option<u32> {
    let endpoint_pid = remote::read_endpoint(data_dir).map(|e| e.pid);
    let remembered_pid = known_app_pid();
    endpoint_pid
        .filter(|&p| is_lisa_process_alive(p))
        .or_else(|| remembered_pid.filter(|&p| is_lisa_process_alive(p)))
        .or_else(|| remembered_pid.filter(|&p| is_process_alive(p)))
        .or_else(|| endpoint_pid.filter(|&p| is_process_alive(p)))
}

/// 引数の中の `--parent <旧pid>` を `new_parent` に差し替える。
pub fn replace_parent_arg(args: &[String], new_parent: Option<u32>) -> Vec<String> {
    let Some(pid) = new_parent else {
        return args.to_vec();
    };
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--parent" && i + 1 < args.len() {
            out.push("--parent".to_string());
            out.push(pid.to_string());
            i += 2;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

/// 自分の新しい `lisa-mcp` を別プロセスとして立ち上げる（`--parent` は今のアプリの pid に差し替える）。
pub fn spawn_replacement_self(data_dir: &Path) -> Result<u32, String> {
    let dir = current_exe_dir()?;
    let exe = dir.join(bin_file_name("lisa-mcp"));
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let args = replace_parent_arg(&raw_args, current_app_pid(data_dir));
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x0800_0000) | DETACHED_PROCESS (0x0000_0008)
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("{} を起動できませんでした: {e}", exe.display()))?;
    Ok(child.id())
}

/// プロセス `pid` が生きていて、しかも Lisa のアプリ（`lisa` か `lisa.exe`、`lisa.old*.exe`）であるか。
pub fn is_lisa_process_alive(pid: u32) -> bool {
    is_process_alive(pid) && process_is_lisa_app(pid)
}

/// `pid` のプロセスの実行ファイルの名前が、Lisa のアプリ（`lisa` か `lisa.exe`、ほかの名前に変えたもの
/// `lisa.old*.exe` も含む）か。分からなければ false（止めない側に倒す）。
pub fn process_is_lisa_app(pid: u32) -> bool {
    process_image_name(pid).is_some_and(|name| is_lisa_app_name(&name))
}

fn is_lisa_app_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == "lisa" || stem.starts_with("lisa.old")
}

/// `pid` の実行ファイルの名前（フォルダを除く）。
fn process_image_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        // SAFETY: 開いたハンドルは必ず閉じ、長さを渡したバッファに書いてもらう。
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut buffer = [0u16; 1024];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut len,
            ) != 0;
            CloseHandle(handle);
            if !ok {
                return None;
            }
            let path = String::from_utf16_lossy(&buffer[..len as usize]);
            Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        }
    }
    #[cfg(not(windows))]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Path::new(&text)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
    }
}

pub fn terminate_process(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("pid が 0 です".into());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return Err(format!(
                    "プロセス {pid} を開けませんでした: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let ok = TerminateProcess(handle, 1) != 0;
            let err = std::io::Error::last_os_error();
            CloseHandle(handle);
            if !ok {
                return Err(format!("プロセス {pid} を止められませんでした: {err}"));
            }
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        let status = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("kill を実行できませんでした: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("プロセス {pid} を止められませんでした（{status}）"))
        }
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

/// ログファイルの最後の `max_lines` 行（最大で末尾 2 MiB 分）と、更新日時・大きさを読む。無ければ `exists: false`。
fn read_log_tail(path: &Path, max_lines: usize) -> Value {
    let Ok(meta) = std::fs::metadata(path) else {
        return json!({
            "path": path,
            "exists": false,
        });
    };
    let size = meta.len();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let read_res = (|| -> std::io::Result<(Vec<u8>, bool)> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let truncated = len > MAX_LOG_READ_BYTES;
        if truncated {
            file.seek(SeekFrom::Start(len - MAX_LOG_READ_BYTES))?;
        }
        let mut buf = Vec::with_capacity(len.min(MAX_LOG_READ_BYTES) as usize);
        file.take(MAX_LOG_READ_BYTES).read_to_end(&mut buf)?;
        Ok((buf, truncated))
    })();
    let Ok((bytes, truncated)) = read_res else {
        return json!({
            "path": path,
            "exists": true,
            "size": size,
            "modified": modified,
            "error": "読めませんでした",
        });
    };
    let slice = if truncated {
        match bytes.iter().position(|&b| b == b'\n') {
            Some(pos) if pos + 1 < bytes.len() => &bytes[pos + 1..],
            _ => &bytes[..],
        }
    } else {
        &bytes[..]
    };
    let content = String::from_utf8_lossy(slice);
    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();
    let start = total_lines.saturating_sub(max_lines);
    let tail: Vec<&str> = all_lines[start..].to_vec();
    json!({
        "path": path,
        "exists": true,
        "size": size,
        "modified": modified,
        "total_lines": total_lines,
        "lines": tail,
        "text": tail.join("\n"),
    })
}

pub const INSTRUCTIONS: &str = "Lisa は、複数の PC やドライブのファイルを読んでハッシュし、重複を見つけるファイラーです。\
DB を読む道具（lisa_drives, lisa_list, lisa_info, lisa_search, lisa_duplicates, lisa_copies, lisa_snapshots）と\
ログや生死を見る道具（lisa_logs, lisa_app_alive）はアプリが起動していなくても使えます。lisa_app_* は起動中のアプリの画面や起動・終了を扱います。\
カタログの道具（lisa_catalogs, lisa_catalog_matches, lisa_unclassified, lisa_outline, lisa_catalog_test）でアプリのフォルダを見分けたり形を調べたりできます。\
整理の道具（lisa_plan_create, lisa_rename, lisa_plan_status, lisa_plans, lisa_plan_cancel, lisa_undo）で移動・フォルダ作成・隔離の計画を出せます。\
計画の許可は人が Lisa の画面（か CLI）で押します。押されるまでユーザーのファイルは何も動きません（許可する道具は無く、lisa_plan_status の wait で待てます）。\
大きさはバイト、日時は UTC の Unix 時刻（ミリ秒）、ハッシュは BLAKE3 の 16 進です。";

/// 道具の一覧（名前、説明、引数の形）。
pub fn tools() -> Value {
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
    let request_undo = json!({
        "name": "lisa_plan_request_undo",
        "description": "ペアリング済みの依頼元が、自分の計画の隔離項目の復元を対象端末に依頼する。対象端末で人が確認するまでファイルは戻さない。",
        "inputSchema": {"type": "object", "properties": {
            "request_id": {"type": "string", "format": "uuid"}
        }, "required": ["request_id"]},
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
    });
    let mut listed = json!([
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
            "name": "lisa_logs",
            "description": "アプリのログ（logs/lisa.log と 1 つ前の lisa.1.log）と crash.log の最後の行、それぞれの更新日時と大きさを返す。無いものは exists: false で返す。",
            "inputSchema": {"type": "object", "properties": {
                "lines": {"type": "integer", "description": "ファイルごとに出す最後の行数（既定 200、最大 2000）"},
            }},
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
        {
            "name": "lisa_update",
            "description": "lisa または lisa-mcp の実行ファイルを HTTP で落として blake3 を確かめ、今のファイルと入れ替える（ペアリングした端末から sidecar にだけ）。url のホストは頼んできた相手の IP と同じであること。",
            "inputSchema": {"type": "object", "properties": {
                "target": {"type": "string", "enum": ["lisa", "lisa-mcp"], "description": "入れ替える対象"},
                "url": {"type": "string", "description": "http://<頼んできた相手の IP>:<port>/... の URL"},
                "blake3": {"type": "string", "description": "BLAKE3 の 16 進（64 文字）"},
                "restart": {"type": "boolean", "description": "入れ替えたあと再起動するか（既定 true）"},
            }, "required": ["target", "url", "blake3"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false},
        },
        {
            "name": "lisa_app_start",
            "description": "同じフォルダのアプリ（lisa.exe / lisa）を起動し、5 秒待ってまだ生きているかを返す（ペアリングした端末から sidecar にだけ）。すでに動いていれば何もしない（restart: true なら止めてから起動）。env で描き方の環境変数（WGPU_BACKEND, LISA_PRESENT_MODE, LISA_UPDATE_MODE, RUST_LOG）を渡せる。",
            "inputSchema": {"type": "object", "properties": {
                "env": {
                    "type": "object",
                    "description": "子プロセスに足す環境変数（キーは WGPU_BACKEND, LISA_PRESENT_MODE, LISA_UPDATE_MODE, RUST_LOG だけ。値は 64 文字まで、英数字と _,=:.- だけ）",
                    "additionalProperties": {"type": "string"},
                },
                "restart": {"type": "boolean", "description": "すでに動いているときに一度止めてから起動し直すか（既定 false）"},
            }},
            "annotations": app_hint,
        },
        {
            "name": "lisa_app_stop",
            "description": "動いているアプリを終了する（ペアリングした端末から sidecar にだけ）。まず Quit を送り、返事が無いときや force: true のときはプロセスを強制終了する。",
            "inputSchema": {"type": "object", "properties": {
                "force": {"type": "boolean", "description": "最初から強制終了するか（既定 false）"},
            }},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "lisa_app_alive",
            "description": "アプリのプロセスが生きているかと、control.json に返事があるかを返す。固まっているのか終わったのかを見分けるため。",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": read_only,
        },
        {
            "name": "lisa_screenshot",
            "description": "OS の画面（または Lisa の窓）を連続で撮って 24 ビット BMP の base64 で返す（ペアリングした端末から sidecar にだけ）。ちらつきなど画面に出たものの切り分け用。",
            "inputSchema": {"type": "object", "properties": {
                "count": {"type": "integer", "description": "撮る枚数（1〜12、既定 6）"},
                "interval_ms": {"type": "integer", "description": "枚と枚の間隔（ミリ秒、30〜2000、既定 80）"},
                "window": {"type": "boolean", "description": "Lisa の窓だけ撮るか（既定 true、false なら画面全体。窓が見つからなければ画面全体）"},
                "scale": {"type": "integer", "description": "縦横をこの数で割って小さくする（1〜4 の整数、既定 2）"},
            }},
            "annotations": read_only,
        },
        {
            "name": "lisa_catalogs",
            "description": "読んでいるカタログ（型）の一覧（ID、名前、アプリ、在りか、当たった数、読めなかったときの理由）。include_text: true で TOML の本文も返す。",
            "inputSchema": {"type": "object", "properties": {
                "include_text": {"type": "boolean", "description": "true なら各型の TOML 本文も返す"},
            }},
            "annotations": read_only,
        },
        {
            "name": "lisa_catalog_matches",
            "description": "型や 1 か所の仕分けに当たったフォルダの一覧と、型ごと・アプリごとの合計。parts: true で中の役割も付ける。",
            "inputSchema": {"type": "object", "properties": {
                "catalog": {"type": "string", "description": "型の ID で絞る"},
                "app": {"type": "string", "description": "アプリ名で絞る"},
                "drive_id": {"type": "string", "description": "ドライブ ID で絞る"},
                "limit": {"type": "integer", "description": "出す数（既定 100、最大 1000）"},
                "totals": {"type": "boolean", "description": "各フォルダの大きさを足すか（既定 true）"},
                "parts": {"type": "boolean", "description": "true なら中の役割（parts）も付ける"},
            }},
            "annotations": read_only,
        },
        {
            "name": "lisa_unclassified",
            "description": "どの型にも仕分けにも当たらない大きなフォルダ（大きさの順、大きな子を含まない一番深いフォルダ）。",
            "inputSchema": {"type": "object", "properties": {
                "under": {"type": "string", "description": "このフォルダの下だけ調べる"},
                "min_size": {"type": "integer", "description": "最小の大きさ（バイト、既定 1 GiB）"},
                "limit": {"type": "integer", "description": "出す数（既定 30、最大 1000）"},
            }},
            "annotations": read_only,
        },
        {
            "name": "lisa_outline",
            "description": "フォルダの形の要約（上から depth 段の木、拡張子ごとの数と大きさ、よく出る名前、小さなテキストファイルの先頭、当たっている型と仕分け）。",
            "inputSchema": with(json!({
                "depth": {"type": "integer", "description": "木を出す段数（既定 3、最大 6）"},
                "text_bytes": {"type": "integer", "description": "小さなテキストの先頭を読むバイト数（既定 400、最大 4000）"},
            })),
            "annotations": read_only,
        },
        {
            "name": "lisa_catalog_test",
            "description": "書きかけのカタログ（TOML 文字列）を保存せずに DB に当て、当たったフォルダと惜しい候補（外れた match が 1 つだけ）を返す。",
            "inputSchema": {"type": "object", "properties": {
                "toml": {"type": "string", "description": "試すカタログの TOML 文字列"},
                "under": {"type": "string", "description": "このフォルダの下だけ調べる"},
                "limit": {"type": "integer", "description": "出す数（既定 50、最大 1000）"},
            }, "required": ["toml"]},
            "annotations": read_only,
        },
        {
            "name": "lisa_catalog_save",
            "description": "カタログの TOML ファイルをユーザーのカタログのフォルダに保存する（新規・上書き）。保存前に形を確かめ、当たりの数も返す。",
            "inputSchema": {"type": "object", "properties": {
                "toml": {"type": "string", "description": "保存するカタログの TOML 文字列"},
                "dir": {"type": "string", "description": "書く先のフォルダ（既定はユーザーのカタログのフォルダ）"},
                "overwrite": {"type": "boolean", "description": "同じ名前があれば上書きするか（既定 false）"},
            }, "required": ["toml"]},
            "annotations": app_hint,
        },
        {
            "name": "lisa_assign",
            "description": "1 か所のフォルダをアプリ用として DB の assignment に仕分ける（同じ場所があれば上書き）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "フォルダの絶対パス"},
                    "id": {"type": "integer", "description": "DB の entry.id（path の代わりに）"},
                    "app": {"type": "string", "description": "アプリ名"},
                    "role": {"type": "string", "enum": ["data", "config", "cache", "install", "log"], "description": "役割（任意）"},
                    "note": {"type": "string", "description": "メモ（任意）"},
                },
                "required": ["app"],
            },
            "annotations": app_hint,
        },
        {
            "name": "lisa_unassign",
            "description": "1 か所の仕分けを DB の assignment から外す（ユーザーのファイルは触らない）。",
            "inputSchema": with(json!({
                "assignment_id": {"type": "integer", "description": "仕分けの行 ID（path / id の代わりに）"},
            })),
            "annotations": app_hint,
        },
        {
            "name": "lisa_rename",
            "description": "同じ親フォルダ内で名前を変更する計画を作る。実行には Lisa で人の許可が必要。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "手元の対象の絶対パス"},
                    "id": {"type": "integer", "description": "対象の DB 上の ID"},
                    "new_name": {"type": "string", "description": "親パスを含めない新しい名前"},
                    "note": {"type": "string", "description": "計画のメモ"},
                    "dry_run": {"type": "boolean", "description": "計画を作らず確認結果だけ返す"}
                },
                "required": ["new_name"],
                "anyOf": [{"required": ["path"]}, {"required": ["id"]}]
            }
        },
        {
            "name": "lisa_plan_create",
            "description": "ファイルの移動・フォルダ作成・隔離の計画を確かめて作る。許可は人が画面（または CLI）で押す。押されるまで何も動かない。refused が 1 つでもあれば計画を作らず理由だけ返す（dry_run: true なら作らず確かめだけ）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ops": {
                        "type": "array",
                        "description": "操作の配列（最大 5000 件）。各要素は {op: \"mkdir\", path}、{op: \"move\", from, to}、{op: \"trash\", path, keep?}、{op: \"trash_linked\", path, decision: {choice: {action: \"delete_actual\"} または {action: \"restore\", link_id}, links: [確認したリンク ID すべて]}} のどれか。マージ済みリンク元がある実体の trash は確認待ちとして理由とリンク ID を返す。パスは絶対パス。",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": {"type": "string", "enum": ["mkdir", "move", "trash", "trash_linked"]},
                                "path": {"type": "string"},
                                "from": {"type": "string"},
                                "to": {"type": "string"},
                                "keep": {"type": "string"},
                            },
                            "required": ["op"],
                        },
                    },
                    "request_id": {"type": "string", "format": "uuid", "description": "ペアリング済み依頼元の再送に使う UUID（任意）"},
                    "note": {"type": "string", "description": "計画の説明（許可画面で人に見せる）"},
                    "dry_run": {"type": "boolean", "description": "true なら計画を作らず確かめだけ行う（既定 false）"},
                },
                "required": ["ops"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
        },
        request_undo,
        {
            "name": "lisa_plan_status",
            "description": "計画の状態と操作ごとの結果を返す。許可は人が画面で押す。wait（秒、最大 60）を付けると、許可待ち（や実行中）が終わるまで待つ。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "計画の番号（lisa_plan_create の id）"},
                    "request_id": {"type": "string", "format": "uuid", "description": "計画の番号の代わりに依頼元が指定した UUID"},
                    "wait": {"type": "integer", "description": "許可待ちが変わるまで待つ秒数（0〜60、既定 0）"},
                    "ops_offset": {"type": "integer", "description": "操作の一覧で飛ばす件数（既定 0）"},
                    "ops_limit": {"type": "integer", "description": "操作の一覧で出す件数（既定 200、最大 5000）"},
                },
                "oneOf": [{"required": ["id"]}, {"required": ["request_id"]}],
            },
            "annotations": read_only,
        },
        {
            "name": "lisa_plans",
            "description": "整理の計画の一覧（新しい順）。状態（state）で絞れる。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": ["pending", "approved", "running", "done", "partial", "rejected", "cancelled", "expired", "undone"],
                        "description": "状態で絞る（省略すれば全部）",
                    },
                    "limit": {"type": "integer", "description": "出す件数（既定 20、最大 200）"},
                },
            },
            "annotations": read_only,
        },
        {
            "name": "lisa_plan_cancel",
            "description": "許可待ち（pending）の計画を取り下げる。すでに実行された計画は取り下げられない（戻すには lisa_undo を使う）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "計画の番号"},
                },
                "required": ["id"],
            },
            "annotations": app_hint,
        },
        {
            "name": "lisa_undo",
            "description": "実行済みの計画（done / partial）を逆の順で取り消す。許可は要らない（上書きはせず、戻す先が埋まっている・元が変わったものは飛ばして理由を返す）。隔離から OS のゴミ箱へ送った分は戻せない。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "計画の番号"},
                },
                "required": ["id"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false},
        },
    ]);
    let link_permits = [
        json!({
            "name": "lisa_merged_link_permits",
            "description": "依頼元のローカル MCP でマージ済みリンクの中継許可を一覧にする。",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": read_only,
        }),
        json!({
            "name": "lisa_merged_link_revoke",
            "description": "依頼元のローカル MCP で指定のリンク中継許可を取り消す。",
            "inputSchema": {"type": "object", "properties": {
                "link_id": {"type": "string", "format": "uuid"}
            }, "required": ["link_id"]},
            "annotations": app_hint,
        }),
    ];
    listed
        .as_array_mut()
        .expect("道具の一覧")
        .extend(link_permits);
    listed
}

/// `sidecar.json` を読む。無いか壊れていれば None。
pub fn read_sidecar(data_dir: &Path) -> Option<SidecarInfo> {
    let bytes = std::fs::read(data_dir.join(SIDECAR_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `sidecar.json` に `(pid, port)` を書く。
pub fn write_sidecar(data_dir: &Path, info: SidecarInfo) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let path = data_dir.join(SIDECAR_FILE);
    let tmp = data_dir.join(format!("{SIDECAR_FILE}.tmp"));
    let bytes = serde_json::to_vec(&info).map_err(err)?;
    write_private(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `sidecar.json` を消す（自分の pid のものだけ）。
pub fn remove_sidecar(data_dir: &Path, pid: u32) {
    if read_sidecar(data_dir).is_some_and(|s| s.pid == pid) {
        let _ = std::fs::remove_file(data_dir.join(SIDECAR_FILE));
    }
}

/// プロセス `pid` がまだ生きているか（Windows は `OpenProcess` + `GetExitCodeProcess`、
/// Unix は `kill -0`）。
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code) != 0;
            CloseHandle(handle);
            ok && exit_code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// 親の `parent_pid` と今のアプリの pid（`known_app_pid`・`control.json`）を `poll_every` ごとに見張り、
/// Lisa のプロセスが消えてから `linger` たったら `handle` を止める。
pub fn watch_parent(
    parent_pid: u32,
    data_dir: &Path,
    linger: std::time::Duration,
    poll_every: std::time::Duration,
    handle: ListenerHandle,
) {
    let mut missing_since: Option<std::time::Instant> = None;
    loop {
        if handle.is_stopped() {
            return;
        }
        let mut live_pid = None;
        if is_lisa_process_alive(parent_pid) {
            live_pid = Some(parent_pid);
        }
        if let Some(kpid) = known_app_pid()
            && is_lisa_process_alive(kpid)
        {
            live_pid = Some(kpid);
        }
        if let Some(endpoint) = remote::read_endpoint(data_dir) {
            if is_lisa_process_alive(endpoint.pid)
                || remote::send(data_dir, Command::State).is_ok_and(|r| r.ok)
            {
                live_pid = Some(endpoint.pid);
            } else {
                let _ = std::fs::remove_file(data_dir.join(remote::FILE_NAME));
            }
        }
        if let Some(pid) = live_pid {
            set_known_app_pid(pid);
            missing_since = None;
        } else {
            let since = *missing_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= linger {
                handle.unblock();
                return;
            }
        }
        let sleep_for = match missing_since {
            Some(since) => linger.saturating_sub(since.elapsed()).min(poll_every),
            None => poll_every,
        };
        if !sleep_for.is_zero() {
            thread::sleep(sleep_for);
        }
    }
}

/// LAN 用の設定（`lan.json`）を読む。無いか壊れていれば空。
pub fn load_lan_config(data_dir: &Path) -> LanConfig {
    let path = data_dir.join(LAN_FILE);
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LanConfig>(&bytes).ok())
        .unwrap_or_default()
}

/// LAN 用の設定（`lan.json`）を保存する。
pub fn save_lan_config(data_dir: &Path, config: &LanConfig) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let path = data_dir.join(LAN_FILE);
    let tmp = data_dir.join(format!("{LAN_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(config).map_err(err)?;
    write_private(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

/// ランダムな合言葉（`xxxx-xxxx-xxxx-xxxx`、19 文字）を作る。
pub fn generate_secret() -> String {
    // v4 の UUID の 13 文字目（版）と 17 文字目（変種）は決まった値なので除き、残りの先頭 16 文字を使う。
    let raw: Vec<char> = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .enumerate()
        .filter(|(i, _)| *i != 12 && *i != 16)
        .map(|(_, c)| c)
        .take(16)
        .collect();
    raw.chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// LAN 用の合言葉を `lan.json` から読むか、無ければ作って保存する（`peers` はそのまま残す）。
pub fn load_or_create_token(data_dir: &Path) -> Result<String, String> {
    let mut config = load_lan_config(data_dir);
    if !config.token.is_empty() {
        return Ok(config.token);
    }
    let token = generate_secret();
    config.token = token.clone();
    save_lan_config(data_dir, &config)?;
    Ok(token)
}

/// ペアリングを許可した相手を `lan.json` の `peers` に覚え（同じ `device_id` があれば上書き）、
/// 作られた `Peer` を返す。
pub fn add_peer(data_dir: &Path, device_id: &str, name: &str) -> Result<Peer, String> {
    let mut config = load_lan_config(data_dir);
    if config.token.is_empty() {
        config.token = generate_secret();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let peer = Peer {
        device_id: device_id.to_string(),
        name: name.to_string(),
        key: generate_secret(),
        added_at: now,
    };
    if let Some(existing) = config.peers.iter_mut().find(|p| p.device_id == device_id) {
        *existing = peer.clone();
    } else {
        config.peers.push(peer.clone());
    }
    save_lan_config(data_dir, &config)?;
    Ok(peer)
}

/// `lan.json` の `peers` から `device_id` の相手を消す。消せたら `true`。
pub fn revoke_peer(data_dir: &Path, device_id: &str) -> Result<bool, String> {
    let mut config = load_lan_config(data_dir);
    let before = config.peers.len();
    config.peers.retain(|p| p.device_id != device_id);
    if config.peers.len() == before {
        return Ok(false);
    }
    save_lan_config(data_dir, &config)?;
    Ok(true)
}

/// 与えられた Bearer の文字列 `given` が、合言葉 `token` か `lan.json` のどれかの `peer.key` に合うか。
pub fn is_authorized(given: &str, token: &str, data_dir: &Path) -> bool {
    resolve_bearer_auth(given, token, data_dir, None).is_some()
}

/// Bearer の文字列 `given` を照合し、`Peer`（ペアリングした端末の key）か `Token`（合言葉）かを返す。
/// どれにも合わなければ `None`。両方を見る時間差が出ないように最後まで比べる。
pub fn resolve_bearer_auth(
    given: &str,
    token: &str,
    data_dir: &Path,
    remote_ip: Option<IpAddr>,
) -> Option<Auth> {
    if given.is_empty() {
        return None;
    }
    let token_ok = !token.is_empty() && constant_time_eq(given.as_bytes(), token.as_bytes());
    let config = load_lan_config(data_dir);
    let mut matched_peer: Option<Peer> = None;
    for peer in &config.peers {
        if !peer.key.is_empty() && constant_time_eq(given.as_bytes(), peer.key.as_bytes()) {
            matched_peer = Some(peer.clone());
        }
    }
    if let Some(peer) = matched_peer {
        Some(Auth::Peer {
            device_id: peer.device_id,
            name: peer.name,
            remote_ip,
        })
    } else if token_ok {
        Some(Auth::Token)
    } else {
        None
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// 2 つの文字列を、長さを含めて一定時間で比べる。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let max = a.len().max(b.len());
    for i in 0..max {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// `HttpListener` を外から止めるためのハンドル。
/// `Weak` で持つので、このハンドルが残っていても `HttpListener::run` が終わればソケットはすぐ閉じる。
#[derive(Clone)]
pub struct ListenerHandle {
    http: Weak<tiny_http::Server>,
    restart_self: Arc<AtomicBool>,
    unblocked: Arc<AtomicBool>,
}

impl ListenerHandle {
    pub fn unblock(&self) {
        self.unblocked.store(true, Ordering::SeqCst);
        if let Some(http) = self.http.upgrade() {
            http.unblock();
        }
    }

    /// 自己再起動の印を付けて待ち受けを止める。
    pub fn request_restart(&self) {
        self.restart_self.store(true, Ordering::SeqCst);
        self.unblock();
    }

    /// 自己再起動が求められたか。
    pub fn restart_requested(&self) -> bool {
        self.restart_self.load(Ordering::SeqCst)
    }

    /// すでに止められたか、`HttpListener` が破棄されたか。
    pub fn is_stopped(&self) -> bool {
        self.unblocked.load(Ordering::SeqCst) || self.http.strong_count() == 0
    }
}

/// 4 桁の数字のペアリング番号を作る。
pub fn generate_pair_code() -> String {
    let n = uuid::Uuid::new_v4().as_u128() % 10_000;
    format!("{n:04}")
}

/// 新しい UUID v4 の文字列を作る。
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

struct CounterGuard(Arc<AtomicUsize>);

impl CounterGuard {
    fn try_acquire(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        let mut cur = counter.load(Ordering::SeqCst);
        loop {
            if cur >= limit {
                return None;
            }
            match counter.compare_exchange_weak(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Self(Arc::clone(counter))),
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// HTTP で待ち受ける口。テストから空いている番号で立ち上げて止められるように分ける。
pub struct HttpListener {
    http: Arc<tiny_http::Server>,
    server: Arc<Server>,
    token: String,
    addr: SocketAddr,
    pair: Option<Arc<PairHandler>>,
    sidecar: bool,
    restart_self: Arc<AtomicBool>,
    unblocked: Arc<AtomicBool>,
}

impl HttpListener {
    pub fn bind(addr: &str, server: Server, token: String) -> Result<Self, String> {
        let mut last_err = String::new();
        let mut http_opt = None;
        for attempt in 0..5 {
            match tiny_http::Server::http(addr) {
                Ok(h) => {
                    http_opt = Some(h);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    if attempt + 1 < 5 {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
        let http = http_opt.ok_or_else(|| format!("{addr} で待てませんでした: {last_err}"))?;
        let bound = http
            .server_addr()
            .to_ip()
            .ok_or_else(|| "待ち受けアドレスが取れませんでした".to_string())?;
        Ok(Self {
            http: Arc::new(http),
            server: Arc::new(server),
            token,
            addr: bound,
            pair: None,
            sidecar: false,
            restart_self: Arc::new(AtomicBool::new(false)),
            unblocked: Arc::new(AtomicBool::new(false)),
        })
    }

    /// ペアリングの頼み（`POST /pair`）を処理する口を付ける。
    pub fn with_pair_handler(mut self, handler: PairHandler) -> Self {
        self.pair = Some(Arc::new(handler));
        self
    }

    /// 別プロセスの `lisa-mcp`（sidecar や `--listen`）として動かす印を付ける。
    /// `false`（アプリの中の 47800）のときは、`lisa_update` / `lisa_app_start` / `lisa_app_stop` を断る。
    pub fn with_sidecar(mut self, sidecar: bool) -> Self {
        self.sidecar = sidecar;
        self
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn handle(&self) -> ListenerHandle {
        ListenerHandle {
            http: Arc::downgrade(&self.http),
            restart_self: Arc::clone(&self.restart_self),
            unblocked: Arc::clone(&self.unblocked),
        }
    }

    /// 届く頼みをスレッドごとにさばき続ける。`unblock` されたら抜ける。
    pub fn run(self) {
        let handle = self.handle();
        let active_requests = Arc::new(AtomicUsize::new(0));
        let active_pairs = Arc::new(AtomicUsize::new(0));
        let active_health = Arc::new(AtomicUsize::new(0));
        let sidecar = self.sidecar;
        for request in self.http.incoming_requests() {
            // DB や画面の応答を待つ頼みで埋まっていても、生存確認用の枠を残す。
            let (active, limit) = if request_path(&request) == "/health" {
                (&active_health, 2)
            } else {
                (&active_requests, MAX_ACTIVE_REQUESTS)
            };
            let Some(req_guard) = CounterGuard::try_acquire(active, limit) else {
                let _ = request.respond(empty(503));
                continue;
            };
            let server = Arc::clone(&self.server);
            let token = self.token.clone();
            let pair = self.pair.clone();
            let handle = handle.clone();
            let active_pairs = Arc::clone(&active_pairs);
            let _ = thread::Builder::new()
                .name("lisa-mcp-http".into())
                .spawn(move || {
                    let _req_guard = req_guard;
                    serve_request(
                        request,
                        &server,
                        &token,
                        pair.as_deref(),
                        &active_pairs,
                        &handle,
                        sidecar,
                    );
                });
        }
    }
}

fn request_path(request: &Request) -> &str {
    request.url().split('?').next().unwrap_or("")
}

fn serve_request(
    mut request: Request,
    server: &Server,
    token: &str,
    pair: Option<&PairHandler>,
    active_pairs: &Arc<AtomicUsize>,
    listener_handle: &ListenerHandle,
    sidecar: bool,
) {
    // Host ヘッダーが IP アドレスか localhost でなければ 403（DNS rebinding 対策、/pair も含む）。
    let host = header(&request, "Host").map(strip_port);
    if !host.is_some_and(is_allowed_host) {
        let _ = request.respond(empty(403));
        return;
    }

    // Origin ヘッダーがあって、その host が Host ヘッダーの host と違えば 403。
    if let Some(origin) = header(&request, "Origin") {
        let origin_host = origin_host(origin);
        if origin_host.is_none() || origin_host != host {
            let _ = request.respond(empty(403));
            return;
        }
    }

    let path = request_path(&request);
    if path == "/health" {
        if *request.method() != Method::Get {
            let _ = request.respond(empty(405));
        } else {
            let _ = request.respond(json_response(200, &json!({"ok": true})));
        }
        return;
    }
    if path == "/pair" {
        serve_pair(request, pair, active_pairs);
        return;
    }

    if path == "/keep-relay" {
        if *request.method() != Method::Post {
            let _ = request.respond(empty(405));
            return;
        }
        let body = match read_body_up_to(&mut request, MAX_TRANSFER_BODY) {
            Ok(body) => body,
            Err(code) => {
                let _ = request.respond(empty(code));
                return;
            }
        };
        let response = match server.handle_keep_relay(&body) {
            Ok(value) => json!({"ok": true, "result": value}),
            Err(message) => json!({"ok": false, "error": message}),
        };
        let _ = request.respond(json_response(200, &response));
        return;
    }

    // 認証: Authorization: Bearer <合言葉または key> が合わなければ 401（何もしない）。
    let given = header(&request, "Authorization")
        .and_then(|v| {
            let (scheme, rest) = v.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("Bearer").then_some(rest.trim())
        })
        .unwrap_or("");
    let remote_ip = request.remote_addr().map(SocketAddr::ip);
    let Some(auth) = resolve_bearer_auth(given, token, &server.data_dir, remote_ip) else {
        let _ = request.respond(empty(401));
        return;
    };

    // パスは `/mcp` だけ。それ以外は 404。
    if path != "/mcp" {
        let _ = request.respond(empty(404));
        return;
    }

    // `/mcp` は POST だけ。それ以外のメソッドは 405。
    if *request.method() != Method::Post {
        let _ = request.respond(empty(405));
        return;
    }

    let body = match read_body_up_to(
        &mut request,
        // 大きな本文はペアリング済みの端末（付け替えの小分け）にだけ認める。
        if matches!(auth, Auth::Peer { .. }) {
            MAX_TRANSFER_BODY
        } else {
            MAX_BODY
        },
    ) {
        Ok(b) => b,
        Err(code) => {
            let _ = request.respond(empty(code));
            return;
        }
    };

    let message: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(error) => {
            let err_json = json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("JSON が読めません: {error}")},
            });
            let _ = request.respond(json_response(200, &err_json));
            return;
        }
    };

    // 配列（バッチ）やオブジェクト以外は 400。
    if !message.is_object() {
        let _ = request.respond(empty(400));
        return;
    }

    match server.handle_in_role(&message, &auth, sidecar) {
        Some(reply) => {
            let restart_self = sidecar
                && reply
                    .get("result")
                    .and_then(|r| r.get("structuredContent"))
                    .and_then(|sc| sc.get("restarting_self"))
                    .and_then(Value::as_bool)
                    == Some(true);
            let _ = request.respond(json_response(200, &reply));
            if restart_self {
                listener_handle.request_restart();
            }
        }
        None => {
            let _ = request.respond(empty(202));
        }
    }
}

fn serve_pair(mut request: Request, pair: Option<&PairHandler>, active_pairs: &Arc<AtomicUsize>) {
    if *request.method() != Method::Post {
        let _ = request.respond(empty(405));
        return;
    }
    let Some(handler) = pair else {
        let _ = request.respond(empty(403));
        return;
    };
    let Some(_pair_guard) = CounterGuard::try_acquire(active_pairs, MAX_ACTIVE_PAIRS) else {
        let _ = request.respond(empty(429));
        return;
    };
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(code) => {
            let _ = request.respond(empty(code));
            return;
        }
    };
    let Ok(mut req) = serde_json::from_slice::<PairRequest>(&body) else {
        let _ = request.respond(empty(400));
        return;
    };
    if !is_valid_peer_field(&req.device_id)
        || !is_valid_peer_field(&req.name)
        || req.code.len() != 4
        || !req.code.bytes().all(|b| b.is_ascii_digit())
    {
        let _ = request.respond(empty(400));
        return;
    }
    req.device_id = req.device_id.trim().to_string();
    req.name = req.name.trim().to_string();
    match handler(req) {
        Some(reply) => {
            let val = serde_json::to_value(reply).unwrap_or(Value::Null);
            let _ = request.respond(json_response(200, &val));
        }
        None => {
            let _ = request.respond(empty(403));
        }
    }
}

/// ペアリングや `here` パケットの `name`・`device_id` が空でなく、64 文字以内で、制御文字や改行を含まないか。
pub fn is_valid_peer_field(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty()
        && text.chars().count() <= MAX_PEER_FIELD_CHARS
        && !text.chars().any(char::is_control)
}

/// `Host` ヘッダーの host 部分（ポート番号を除いたもの）が IP アドレスか `localhost` か。
fn is_allowed_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<IpAddr>().is_ok()
}

/// 本文を `MAX_BODY`（1 MiB）まで読む。超えれば `Err(413)`、読めなければ `Err(400)`。
fn read_body(request: &mut Request) -> Result<Vec<u8>, u16> {
    read_body_up_to(request, MAX_BODY)
}

/// 本文を `limit` バイトまで読む。超えれば `Err(413)`、読めなければ `Err(400)`。
fn read_body_up_to(request: &mut Request, limit: usize) -> Result<Vec<u8>, u16> {
    if request.body_length().is_some_and(|len| len > limit) {
        return Err(413);
    }
    let mut body = Vec::new();
    if request
        .as_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut body)
        .is_err()
    {
        return Err(400);
    }
    if body.len() > limit {
        return Err(413);
    }
    Ok(body)
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

/// `Origin` の値（`http://host:port` など）から host 部分だけを取り出す。
fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.trim().split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = strip_port(host_port);
    (!host.is_empty()).then_some(host)
}

/// `host:port` や `[::1]:port` から `:port` を外した host 部分。
fn strip_port(host_port: &str) -> &str {
    let s = host_port.trim();
    if let Some(rest) = s.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &s[..=end + 1];
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => s,
    }
}

fn empty(code: u16) -> Response<std::io::Empty> {
    Response::empty(StatusCode(code))
}

fn json_response(code: u16, value: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(value).expect("JSON にできる");
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("ヘッダー");
    Response::from_data(bytes)
        .with_status_code(StatusCode(code))
        .with_header(header)
}

/// UDP の `hello` パケットかどうかを確かめる。
pub fn is_hello_packet(bytes: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    v.get("lisa").and_then(Value::as_str) == Some("hello")
        && v.get("v").and_then(Value::as_i64) == Some(1)
}

/// UDP の `here` パケットを組み立てる。`mcp_port` があれば `"mcp_port"` も足す。
pub fn make_here_packet(device_id: &str, name: &str, port: u16, mcp_port: Option<u16>) -> Vec<u8> {
    let mut v = json!({
        "lisa": "here",
        "v": 1,
        "device_id": device_id,
        "name": name,
        "port": port,
    });
    if let Some(mcp_port) = mcp_port.filter(|&p| p != 0) {
        v["mcp_port"] = json!(mcp_port);
    }
    serde_json::to_vec(&v).unwrap_or_default()
}

/// UDP の `here` パケットから `(device_id, name, port, mcp_port)` を取り出す。
pub fn parse_here_packet(bytes: &[u8]) -> Option<(String, String, u16, Option<u16>)> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    if v.get("lisa").and_then(Value::as_str) != Some("here")
        || v.get("v").and_then(Value::as_i64) != Some(1)
    {
        return None;
    }
    let raw_id = v.get("device_id")?.as_str()?;
    let raw_name = v.get("name")?.as_str()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    if !is_valid_peer_field(raw_id) || !is_valid_peer_field(raw_name) || port == 0 {
        return None;
    }
    let device_id = raw_id.trim().to_string();
    let name = raw_name.trim().to_string();
    let mcp_port = v
        .get("mcp_port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|&p| p != 0);
    Some((device_id, name, port, mcp_port))
}

/// UDP の `hello` パケット。
pub fn make_hello_packet() -> Vec<u8> {
    serde_json::to_vec(&json!({"lisa": "hello", "v": 1})).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("lisa-mcp-test-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 1 回だけ HTTP の要求を送り、（ステータスコード, 本文）を返す。
    fn send_raw(addr: SocketAddr, request: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).unwrap();
        let code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        (code, String::from_utf8(body).unwrap())
    }

    #[test]
    fn http_mcp_server() {
        let dir = TempDir::new("http");
        // DB に端末名を入れておく。
        {
            let mut db = Db::open(&dir.0.join("lisa.db")).unwrap();
            db.this_device("win-pc", "win-pc", "windows").unwrap();
        }
        let token = load_or_create_token(&dir.0).unwrap();
        assert_eq!(token.len(), 19);
        assert_eq!(load_or_create_token(&dir.0).unwrap(), token);

        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir.0.clone(),
                cache_dir: None,
            },
            token.clone(),
        )
        .unwrap();
        let addr = listener.local_addr();
        let handle = listener.handle();
        let thread = thread::spawn(move || listener.run());

        // 1. 合言葉なし → 401
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        );
        assert_eq!(code, 401);

        // 2. 合言葉が違う → 401
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer wrong\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        );
        assert_eq!(code, 401);

        // 3. 正しい合言葉 → initialize が 200 で返り、端末名が入っている
        let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let (code, body) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{init_body}",
                init_body.len()
            ),
        );
        assert_eq!(code, 200);
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["result"]["serverInfo"]["name"], "lisa (win-pc)");
        assert!(
            reply["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("win-pc")
        );

        // 4. 知らせ（id が無い）→ 202
        let notif_body = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let (code, body) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{notif_body}",
                notif_body.len()
            ),
        );
        assert_eq!(code, 202);
        assert!(body.is_empty());

        // 5. 違うパス → 404
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /other HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(code, 404);

        // 6. Origin が Host と違う → 403
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://evil.example.com\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        );
        assert_eq!(code, 403);

        // Origin の host が Host と同じなら通る
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://127.0.0.1:9999\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{notif_body}",
                notif_body.len()
            ),
        );
        assert_eq!(code, 202);

        // 7. POST 以外のメソッド → 405
        let (code, _) = send_raw(
            addr,
            &format!(
                "GET /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(code, 405);

        // 8. 配列（バッチ）→ 400
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]"
            ),
        );
        assert_eq!(code, 400);

        // 9. JSON が壊れている → 200 で -32700
        let (code, body) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{{bad"
            ),
        );
        assert_eq!(code, 200);
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["error"]["code"], -32700);

        // 10. 画面なし（pair handler なし）の /pair → 403
        let pair_body = r#"{"device_id":"d1","name":"mac","code":"1234"}"#;
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /pair HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{pair_body}",
                pair_body.len()
            ),
        );
        assert_eq!(code, 403);

        handle.unblock();
        thread.join().unwrap();
    }

    #[test]
    fn lan_peers_auth_and_pairing() {
        let dir = TempDir::new("pair");
        let dir_path = dir.0.clone();
        {
            let mut db = Db::open(&dir_path.join("lisa.db")).unwrap();
            db.this_device("host-pc", "host-pc", "macos").unwrap();
        }
        let token = load_or_create_token(&dir_path).unwrap();

        let dir_for_handler = dir_path.clone();
        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_path.clone(),
                cache_dir: None,
            },
            token.clone(),
        )
        .unwrap()
        .with_pair_handler(Box::new(move |req| {
            if req.code == "1234" {
                let peer = add_peer(&dir_for_handler, &req.device_id, &req.name).ok()?;
                Some(PairReply {
                    key: peer.key,
                    device_id: "host-id".into(),
                    name: "host-pc".into(),
                })
            } else {
                None
            }
        }));
        let addr = listener.local_addr();
        let handle = listener.handle();
        let thread = thread::spawn(move || listener.run());

        // 断る（番号が 9999）→ 403
        let reject_body = r#"{"device_id":"client-1","name":"client","code":"9999"}"#;
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /pair HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reject_body}",
                reject_body.len()
            ),
        );
        assert_eq!(code, 403);

        // 許可（番号が 1234）→ 200 で key が返り、lan.json に保存される
        let allow_body = r#"{"device_id":"client-1","name":"client","code":"1234"}"#;
        let (code, body) = send_raw(
            addr,
            &format!(
                "POST /pair HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{allow_body}",
                allow_body.len()
            ),
        );
        assert_eq!(code, 200);
        let reply: PairReply = serde_json::from_str(&body).unwrap();
        assert_eq!(reply.device_id, "host-id");
        assert_eq!(reply.name, "host-pc");
        assert_eq!(reply.key.len(), 19);

        let config = load_lan_config(&dir_path);
        assert_eq!(config.token, token);
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].device_id, "client-1");
        assert_eq!(config.peers[0].key, reply.key);

        // key でも token でも /mcp の tools/call lisa_drives が通る
        let call_body =
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"lisa_drives"}}"#;
        for secret in [&token, &reply.key] {
            let (code, body) = send_raw(
                addr,
                &format!(
                    "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call_body}",
                    call_body.len()
                ),
            );
            assert_eq!(code, 200);
            let res: Value = serde_json::from_str(&body).unwrap();
            assert!(res["result"]["structuredContent"]["drives"].is_array());
        }

        // 取り消した key は 401 になり、token はそのまま通る
        assert!(revoke_peer(&dir_path, "client-1").unwrap());
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call_body}",
                reply.key,
                call_body.len()
            ),
        );
        assert_eq!(code, 401);
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call_body}",
                call_body.len()
            ),
        );
        assert_eq!(code, 200);

        handle.unblock();
        thread.join().unwrap();
    }

    #[test]
    fn udp_hello_and_here_packets() {
        use std::net::UdpSocket;
        use std::time::Duration;

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, src) = server_sock.recv_from(&mut buf).unwrap();
            assert!(is_hello_packet(&buf[..n]));
            let here = make_here_packet("dev-123", "My Mac", 47800, Some(47802));
            server_sock.send_to(&here, src).unwrap();
        });

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let hello = make_hello_packet();
        client_sock.send_to(&hello, server_addr).unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = client_sock.recv_from(&mut buf).unwrap();
        let parsed = parse_here_packet(&buf[..n]).unwrap();
        assert_eq!(
            parsed,
            ("dev-123".into(), "My Mac".into(), 47800, Some(47802))
        );
        handle.join().unwrap();

        // mcp_port 無しの古いパケットも読める。
        let no_mcp = make_here_packet("dev-123", "My Mac", 47800, None);
        assert_eq!(
            parse_here_packet(&no_mcp).unwrap(),
            ("dev-123".into(), "My Mac".into(), 47800, None)
        );
    }

    #[test]
    fn lisa_logs_missing_and_present() {
        let dir = TempDir::new("logs");
        let server = Server {
            data_dir: dir.0.clone(),
            cache_dir: None,
        };

        // 1. 何も無いとき → どれも exists: false で、エラーにはならない
        let reply = server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "lisa_logs"},
            }))
            .unwrap();
        let sc = &reply["result"]["structuredContent"];
        assert_eq!(sc["lines"], 200);
        assert_eq!(sc["lisa_log"]["exists"], false);
        assert_eq!(sc["lisa_1_log"]["exists"], false);
        assert_eq!(sc["crash_log"]["exists"], false);

        // 2. logs/lisa.log と crash.log を作って lines: 2 で呼ぶ → 最後の 2 行と大きさ・更新日時が返る
        let logs_dir = dir.0.join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(
            logs_dir.join("lisa.log"),
            "line 1\nline 2\nline 3\nline 4\n",
        )
        .unwrap();
        std::fs::write(dir.0.join("crash.log"), "crash A\ncrash B\ncrash C\n").unwrap();

        let reply = server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "lisa_logs", "arguments": {"lines": 2}},
            }))
            .unwrap();
        let sc = &reply["result"]["structuredContent"];
        assert_eq!(sc["lines"], 2);
        assert_eq!(sc["lisa_log"]["exists"], true);
        assert_eq!(sc["lisa_log"]["total_lines"], 4);
        assert_eq!(sc["lisa_log"]["lines"], json!(["line 3", "line 4"]));
        assert_eq!(sc["lisa_log"]["text"], "line 3\nline 4");
        assert!(sc["lisa_log"]["size"].as_u64().unwrap() > 0);
        assert!(sc["lisa_log"]["modified"].as_i64().unwrap() > 0);

        assert_eq!(sc["lisa_1_log"]["exists"], false);

        assert_eq!(sc["crash_log"]["exists"], true);
        assert_eq!(sc["crash_log"]["total_lines"], 3);
        assert_eq!(sc["crash_log"]["lines"], json!(["crash B", "crash C"]));
        assert!(sc["crash_log"]["size"].as_u64().unwrap() > 0);
        assert!(sc["crash_log"]["modified"].as_i64().unwrap() > 0);
    }

    #[test]
    fn health_sidecar_and_parent_linger() {
        let dir = TempDir::new("sidecar");
        let dir_path = dir.0.clone();
        let token = load_or_create_token(&dir_path).unwrap();

        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_path.clone(),
                cache_dir: None,
            },
            token,
        )
        .unwrap();
        let addr = listener.local_addr();
        let handle = listener.handle();
        let pid = std::process::id();
        write_sidecar(
            &dir_path,
            SidecarInfo {
                pid,
                port: addr.port(),
            },
        )
        .unwrap();
        assert_eq!(
            read_sidecar(&dir_path),
            Some(SidecarInfo {
                pid,
                port: addr.port()
            })
        );

        // 存在しない親の pid と linger 1 秒を渡すと、数秒でサーバーが終わって sidecar.json も消える。
        let dir_for_server = dir_path.clone();
        let server_thread = thread::spawn(move || {
            listener.run();
            remove_sidecar(&dir_for_server, pid);
        });

        // 1. GET /health は認証なしで 200 {"ok":true}
        let (code, body) = send_raw(
            addr,
            &format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v, json!({"ok": true}));

        // POST /health は 405
        let (code, _) = send_raw(
            addr,
            &format!(
                "POST /health HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(code, 405);

        // 今のプロセスは生きている、存在しない大きな pid は生きていない。
        assert!(is_process_alive(pid));
        let bogus_pid: u32 = 999_999_999;
        assert!(!is_process_alive(bogus_pid));

        let started = std::time::Instant::now();
        watch_parent(
            bogus_pid,
            &dir_path,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
            handle,
        );
        server_thread.join().unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(900)
                && elapsed < std::time::Duration::from_secs(5),
            "elapsed: {elapsed:?}"
        );
        assert!(!dir_path.join(SIDECAR_FILE).exists());
    }

    #[test]
    fn only_lisa_app_names_are_stopped() {
        assert!(is_lisa_app_name("lisa.exe"));
        assert!(is_lisa_app_name("LISA.EXE"));
        assert!(is_lisa_app_name("lisa"));
        assert!(is_lisa_app_name("lisa.old.exe"));
        assert!(!is_lisa_app_name("lisa-mcp.exe"));
        assert!(!is_lisa_app_name("explorer.exe"));
        // このテストのプロセス自身は Lisa ではない。
        assert!(!process_is_lisa_app(std::process::id()));
        assert!(!is_lisa_process_alive(std::process::id()));
    }

    #[test]
    fn peer_only_tools_and_update_binary_in() {
        let dir = TempDir::new("update");
        let dir_path = dir.0.clone();
        let token = load_or_create_token(&dir_path).unwrap();
        let peer = add_peer(&dir_path, "mac-1", "Mac").unwrap();

        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_path.clone(),
                cache_dir: None,
            },
            token.clone(),
        )
        .unwrap()
        .with_sidecar(true);
        let addr = listener.local_addr();
        let handle = listener.handle();
        let server_thread = thread::spawn(move || listener.run());

        // 1. stdio や合言葉（token）では lisa_update / lisa_app_start / lisa_app_stop / lisa_screenshot が断られ、
        //    lisa_app_alive は通る
        for tool in [
            "lisa_update",
            "lisa_app_start",
            "lisa_app_stop",
            "lisa_screenshot",
        ] {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": tool, "arguments": {}},
            })
            .to_string();
            let (code, resp) = send_raw(
                addr,
                &format!(
                    "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
            );
            assert_eq!(code, 200);
            let v: Value = serde_json::from_str(&resp).unwrap();
            assert_eq!(
                v["result"]["isError"], true,
                "tool {tool} should fail with token"
            );
            assert!(
                v["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("ペアリングした端末からだけ")
            );
        }

        // token でも lisa_app_alive は通る
        {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": {"name": "lisa_app_alive", "arguments": {}},
            })
            .to_string();
            let (code, resp) = send_raw(
                addr,
                &format!(
                    "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
            );
            assert_eq!(code, 200);
            let v: Value = serde_json::from_str(&resp).unwrap();
            assert_ne!(v["result"]["isError"], json!(true));
        }

        // 2. peer の key なら lisa_app_alive と lisa_app_stop が通る
        for tool in ["lisa_app_alive", "lisa_app_stop"] {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": tool, "arguments": {}},
            })
            .to_string();
            let (code, resp) = send_raw(
                addr,
                &format!(
                    "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    peer.key,
                    body.len()
                ),
            );
            assert_eq!(code, 200);
            let v: Value = serde_json::from_str(&resp).unwrap();
            assert_ne!(
                v["result"]["isError"],
                json!(true),
                "tool {tool} should succeed with peer key: {v}"
            );
        }

        handle.unblock();
        server_thread.join().unwrap();

        // 2b. アプリの中の口（with_sidecar(false)）では peer の key でも危ない道具が断られる
        let app_listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_path.clone(),
                cache_dir: None,
            },
            token.clone(),
        )
        .unwrap();
        let app_addr = app_listener.local_addr();
        let app_handle = app_listener.handle();
        let app_thread = thread::spawn(move || app_listener.run());

        for tool in [
            "lisa_update",
            "lisa_app_start",
            "lisa_app_stop",
            "lisa_screenshot",
        ] {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": tool, "arguments": {}},
            })
            .to_string();
            let (code, resp) = send_raw(
                app_addr,
                &format!(
                    "POST /mcp HTTP/1.1\r\nHost: {app_addr}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    peer.key,
                    body.len()
                ),
            );
            assert_eq!(code, 200);
            let v: Value = serde_json::from_str(&resp).unwrap();
            assert_eq!(v["result"]["isError"], true);
            assert!(
                v["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("sidecar（47802）に頼んでください")
            );
        }

        app_handle.unblock();
        app_thread.join().unwrap();

        // 3. update_binary_in: 一時フォルダに偽の exe を置き、HTTP サーバーから落として入れ替える
        let bin_dir = dir_path.join("bins");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe_path = bin_dir.join(bin_file_name("lisa"));
        let old_bytes = b"old-lisa-binary";
        std::fs::write(&exe_path, old_bytes).unwrap();
        let old_hash = query::hex(
            lisa_core::hash::Hasher::new()
                .update(old_bytes)
                .finalize()
                .as_bytes(),
        );

        let new_bytes = b"new-lisa-binary-v2-longer";
        let new_hash = query::hex(
            lisa_core::hash::Hasher::new()
                .update(new_bytes)
                .finalize()
                .as_bytes(),
        );

        // 一時の HTTP サーバーを 127.0.0.1:0 で立ち上げ、/redirect は 302、それ以外は new_bytes を返す
        let file_http = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let file_port = file_http.server_addr().to_ip().unwrap().port();
        let file_http_bg = Arc::clone(&file_http);
        let file_thread = thread::spawn(move || {
            for req in file_http_bg.incoming_requests() {
                if req.url() == "/redirect" {
                    let loc =
                        Header::from_bytes(&b"Location"[..], &b"http://127.0.0.1/lisa.exe"[..])
                            .unwrap();
                    let _ = req.respond(Response::empty(StatusCode(302)).with_header(loc));
                } else {
                    let _ = req.respond(Response::from_data(new_bytes.to_vec()));
                }
            }
        });
        let file_url = format!("http://127.0.0.1:{file_port}/lisa.exe");

        // 3a. url のホストが相手の IP と違えば断り、元のファイルはそのまま
        let wrong_ip: IpAddr = "192.0.2.99".parse().unwrap();
        let err =
            update_binary_in(&bin_dir, "lisa", &file_url, &new_hash, Some(wrong_ip)).unwrap_err();
        assert!(
            err.contains("IP") && err.contains("違います"),
            "unexpected err: {err}"
        );
        assert_eq!(std::fs::read(&exe_path).unwrap(), old_bytes);
        assert!(!bin_dir.join("lisa.new").exists());

        // 3a-2. expected_ip が None でも断る
        let err = update_binary_in(&bin_dir, "lisa", &file_url, &new_hash, None).unwrap_err();
        assert!(err.contains("IP"), "unexpected err: {err}");

        // 3a-3. HTTP リダイレクト（302）は追跡せず断る
        let redirect_url = format!("http://127.0.0.1:{file_port}/redirect");
        let err = update_binary_in(
            &bin_dir,
            "lisa",
            &redirect_url,
            &new_hash,
            Some("127.0.0.1".parse().unwrap()),
        )
        .unwrap_err();
        assert!(!err.is_empty());

        // 3b. http:// 以外の URL は断る
        let err = update_binary_in(
            &bin_dir,
            "lisa",
            &format!("https://127.0.0.1:{file_port}/lisa.exe"),
            &new_hash,
            Some("127.0.0.1".parse().unwrap()),
        )
        .unwrap_err();
        assert!(err.contains("http://"));

        // 3c. blake3 が違えば .new を消して失敗し、元のファイルはそのまま
        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = update_binary_in(
            &bin_dir,
            "lisa",
            &file_url,
            wrong_hash,
            Some("127.0.0.1".parse().unwrap()),
        )
        .unwrap_err();
        assert!(err.contains("blake3 が合いません"), "unexpected err: {err}");
        assert_eq!(std::fs::read(&exe_path).unwrap(), old_bytes);
        assert!(!bin_dir.join("lisa.new").exists());

        // 3c-2. before_replace が失敗したら（アプリを止められなかったとき）入れ替えずに中断する
        let err = update_binary_in_with(
            &bin_dir,
            "lisa",
            &file_url,
            &new_hash,
            Some("127.0.0.1".parse().unwrap()),
            || Err("止められませんでした".into()),
        )
        .unwrap_err();
        assert!(err.contains("止められませんでした"));
        assert_eq!(std::fs::read(&exe_path).unwrap(), old_bytes);
        assert!(!bin_dir.join("lisa.new").exists());

        // 3d. 古い *.old-<ts>.exe があれば掃除し、blake3 と IP が合えば入れ替わる
        let stale_old = bin_dir.join(old_bin_file_name("lisa", Some(12345)));
        std::fs::write(&stale_old, b"stale").unwrap();
        let outcome = update_binary_for_test(&bin_dir, "lisa", &file_url, &new_hash).unwrap();
        assert!(!stale_old.exists(), "stale old binary should be cleaned up");
        assert_eq!(outcome.path, exe_path);
        assert_eq!(outcome.old_size, Some(old_bytes.len() as u64));
        assert_eq!(outcome.old_blake3.as_deref(), Some(old_hash.as_str()));
        assert_eq!(outcome.new_size, new_bytes.len() as u64);
        assert_eq!(outcome.new_blake3, new_hash);
        assert_eq!(std::fs::read(&exe_path).unwrap(), new_bytes);
        let old_path = outcome.old_path.unwrap();
        assert_eq!(std::fs::read(&old_path).unwrap(), old_bytes);
        assert!(!bin_dir.join("lisa.new").exists());

        file_http.unblock();
        file_thread.join().unwrap();
    }

    #[test]
    fn host_validation_token_override_and_pair_limits() {
        let dir = TempDir::new("security");
        let dir_path = dir.0.clone();
        let saved_token = load_or_create_token(&dir_path).unwrap();
        let custom_token = "custom-override-token".to_string();

        // --token で別の合言葉を渡したとき、lan.json の古い token は通らない（R1-9）
        assert_eq!(
            resolve_bearer_auth(&saved_token, &custom_token, &dir_path, None),
            None
        );
        assert_eq!(
            resolve_bearer_auth(&custom_token, &custom_token, &dir_path, None),
            Some(Auth::Token)
        );

        let (barrier_tx, barrier_rx) = std::sync::mpsc::channel::<()>();
        let barrier_rx = Arc::new(Mutex::new(barrier_rx));
        let entered = Arc::new(AtomicUsize::new(0));
        let entered_bg = Arc::clone(&entered);
        let rx_bg = Arc::clone(&barrier_rx);

        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_path.clone(),
                cache_dir: None,
            },
            custom_token.clone(),
        )
        .unwrap()
        .with_pair_handler(Box::new(move |_req| {
            entered_bg.fetch_add(1, Ordering::SeqCst);
            let _ = rx_bg.lock().unwrap().recv_timeout(Duration::from_secs(5));
            None
        }));
        let addr = listener.local_addr();
        let handle = listener.handle();
        let server_thread = thread::spawn(move || listener.run());

        // Host がドメイン名（DNS rebinding）なら /mcp も /pair も /health も 403（R1-10）
        for path in ["/mcp", "/pair", "/health"] {
            let (code, _) = send_raw(
                addr,
                &format!(
                    "POST {path} HTTP/1.1\r\nHost: rebind.attacker.example:{}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
                    addr.port()
                ),
            );
            assert_eq!(code, 403, "path {path} with domain Host should be 403");
        }
        // Host: localhost:<port> は通る
        let (code, _) = send_raw(
            addr,
            &format!(
                "GET /health HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
                addr.port()
            ),
        );
        assert_eq!(code, 200);

        // /pair の name・device_id のバリデーション（64 文字超、改行・制御文字は 400）（R1-6）
        let long_name = "a".repeat(65);
        for bad_body in [
            json!({"device_id": "d1", "name": long_name, "code": "1234"}).to_string(),
            json!({"device_id": "d1\nbad", "name": "mac", "code": "1234"}).to_string(),
            json!({"device_id": "d1", "name": "mac\r\n偽の文", "code": "1234"}).to_string(),
        ] {
            let (code, _) = send_raw(
                addr,
                &format!(
                    "POST /pair HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bad_body}",
                    bad_body.len()
                ),
            );
            assert_eq!(code, 400);
        }
        assert!(parse_here_packet(&make_here_packet("id", &long_name, 47800, None)).is_none());
        assert!(parse_here_packet(&make_here_packet("id", "bad\nname", 47800, None)).is_none());

        // /pair の同時待ちは MAX_ACTIVE_PAIRS（4）まで。5 件目は即 429 で断られる（R1-6）
        let valid_pair = r#"{"device_id":"d1","name":"mac","code":"1234"}"#;
        let mut waiters = Vec::new();
        for _ in 0..MAX_ACTIVE_PAIRS {
            let req_str = format!(
                "POST /pair HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{valid_pair}",
                valid_pair.len()
            );
            waiters.push(thread::spawn(move || send_raw(addr, &req_str)));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while entered.load(Ordering::SeqCst) < MAX_ACTIVE_PAIRS
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(entered.load(Ordering::SeqCst), MAX_ACTIVE_PAIRS);

        let (fifth_code, _) = send_raw(
            addr,
            &format!(
                "POST /pair HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{valid_pair}",
                valid_pair.len()
            ),
        );
        assert_eq!(fifth_code, 429);

        for _ in 0..MAX_ACTIVE_PAIRS {
            let _ = barrier_tx.send(());
        }
        for w in waiters {
            let (code, _) = w.join().unwrap();
            assert_eq!(code, 403);
        }

        // request_restart で listener.run() が抜け、handle を握ったままでも同じポートをすぐ再 bind できる（R1-1）
        handle.request_restart();
        server_thread.join().unwrap();
        assert!(handle.restart_requested());
        let rebound = HttpListener::bind(
            &addr.to_string(),
            Server {
                data_dir: dir_path.clone(),
                cache_dir: None,
            },
            custom_token,
        );
        assert!(
            rebound.is_ok(),
            "port should be released even while ListenerHandle is alive"
        );
    }

    #[test]
    fn stale_control_json_and_log_tail_2mib() {
        let dir = TempDir::new("stale");
        let dir_path = dir.0.clone();
        set_known_app_pid(0);

        // 今のテストプロセス自身（Lisa ではない）の pid が書かれた古い control.json を置く（R1-4）
        let stale_control = dir_path.join(remote::FILE_NAME);
        std::fs::write(
            &stale_control,
            serde_json::to_vec(&remote::Endpoint {
                port: 1,
                token: "stale".into(),
                pid: std::process::id(),
            })
            .unwrap(),
        )
        .unwrap();

        let server = Server {
            data_dir: dir_path.clone(),
            cache_dir: None,
        };
        // app_alive は古い control.json を消し、process_alive: false を返す
        let alive = server.app_alive().unwrap();
        assert_eq!(alive["process_alive"], false);
        assert_eq!(alive["responding"], false);
        assert_eq!(alive["control_json_exists"], false);
        assert!(!stale_control.exists());

        // app_stop も古い control.json でエラーにならず掃除して返す
        std::fs::write(
            &stale_control,
            serde_json::to_vec(&remote::Endpoint {
                port: 1,
                token: "stale".into(),
                pid: std::process::id(),
            })
            .unwrap(),
        )
        .unwrap();
        let stop = server.app_stop(&json!({})).unwrap();
        assert_eq!(stop["alive"], false);
        assert!(!stale_control.exists());

        // replace_parent_arg のテスト（R1-1）
        let args = vec![
            "--listen".to_string(),
            "0.0.0.0:47802".to_string(),
            "--parent".to_string(),
            "100".to_string(),
        ];
        assert_eq!(
            replace_parent_arg(&args, Some(200)),
            vec!["--listen", "0.0.0.0:47802", "--parent", "200"]
        );
        assert_eq!(replace_parent_arg(&args, None), args);

        // read_log_tail が 2 MiB 超のログでも末尾だけ読める（R1-11）
        let big_log = dir_path.join("big.log");
        let mut content = "x".repeat(2 * 1024 * 1024 + 500);
        content.push_str("\nlast-line-1\nlast-line-2\n");
        std::fs::write(&big_log, content.as_bytes()).unwrap();
        let tail = read_log_tail(&big_log, 2);
        assert_eq!(tail["exists"], true);
        assert_eq!(tail["lines"], json!(["last-line-1", "last-line-2"]));

        // CounterGuard: 上限まで取れて、Drop されると元の数に戻り再び取れる
        let counter = Arc::new(AtomicUsize::new(0));
        let mut guards: Vec<CounterGuard> = (0..MAX_ACTIVE_REQUESTS)
            .map(|_| CounterGuard::try_acquire(&counter, MAX_ACTIVE_REQUESTS).unwrap())
            .collect();
        assert_eq!(counter.load(Ordering::SeqCst), MAX_ACTIVE_REQUESTS);
        assert!(CounterGuard::try_acquire(&counter, MAX_ACTIVE_REQUESTS).is_none());
        guards.pop();
        assert_eq!(counter.load(Ordering::SeqCst), MAX_ACTIVE_REQUESTS - 1);
        assert!(CounterGuard::try_acquire(&counter, MAX_ACTIVE_REQUESTS).is_some());
        drop(guards);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn app_start_env_validation() {
        // env 無し・null・空オブジェクトは通る
        assert_eq!(parse_app_start_env(&json!({})).unwrap(), vec![]);
        assert_eq!(parse_app_start_env(&json!({"env": null})).unwrap(), vec![]);
        assert_eq!(parse_app_start_env(&json!({"env": {}})).unwrap(), vec![]);

        // 許される 4 つのキーと記号（_,=:.-）
        let ok = parse_app_start_env(&json!({
            "env": {
                "WGPU_BACKEND": "dx12",
                "LISA_PRESENT_MODE": "fifo_relaxed",
                "LISA_UPDATE_MODE": "continuous",
                "RUST_LOG": "info,wgpu_hal=debug,lisa::view=trace:1.0-rc",
            }
        }))
        .unwrap();
        assert_eq!(ok.len(), 4);

        // 64 文字ちょうどは通り、65 文字は断る
        let v64 = "a".repeat(64);
        assert!(validate_app_env_pair("WGPU_BACKEND", &v64).is_ok());
        let v65 = "a".repeat(65);
        assert!(validate_app_env_pair("WGPU_BACKEND", &v65).is_err());

        // 許されないキーは断る
        for bad_key in [
            "PATH",
            "LD_PRELOAD",
            "LISA_DATA_DIR",
            "RUST_BACKTRACE",
            "wgpu_backend",
            "",
        ] {
            assert!(
                validate_app_env_pair(bad_key, "dx12").is_err(),
                "key {bad_key:?} should be rejected"
            );
        }

        // 許されない文字（空白、スラッシュ、引用符、セミコロン、日本語など）は断る
        for bad_val in [
            "dx12; rm -rf",
            "continuous ",
            "/tmp/x",
            "\"fifo\"",
            "fifo\n",
            "あいう",
        ] {
            assert!(
                validate_app_env_pair("LISA_PRESENT_MODE", bad_val).is_err(),
                "value {bad_val:?} should be rejected"
            );
        }

        // 文字列でない値や配列は断る
        assert!(parse_app_start_env(&json!({"env": "not-an-object"})).is_err());
        assert!(parse_app_start_env(&json!({"env": {"WGPU_BACKEND": 123}})).is_err());
    }

    #[test]
    fn screenshot_bmp_args_and_40mb_limit() {
        // 1. 小さな絵（3x2、1 行 9 バイト → 4 バイト境界で 12 バイトに 0 埋め）を 24 ビット BMP にして、
        //    見出しの大きさ（54 バイト）と中身（下の行から BGR 順＋パディング）を確かめる
        let rgb_3x2: [u8; 18] = [
            // 上の行 (y = 0): 赤(10,20,30), 緑(40,50,60), 青(70,80,90)
            10, 20, 30, 40, 50, 60, 70, 80, 90,
            // 下の行 (y = 1): 白(100,110,120), 灰(130,140,150), 黒(160,170,180)
            100, 110, 120, 130, 140, 150, 160, 170, 180,
        ];
        let bmp = encode_bmp24(3, 2, &rgb_3x2).unwrap();
        // ファイル全体は 54 (見出し) + 12 * 2 (画素データ) = 78 バイト
        assert_eq!(bmp.len(), 78);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[2..6].try_into().unwrap()), 78);
        assert_eq!(u32::from_le_bytes(bmp[6..10].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(bmp[10..14].try_into().unwrap()), 54);
        assert_eq!(u32::from_le_bytes(bmp[14..18].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(bmp[18..22].try_into().unwrap()), 3);
        assert_eq!(i32::from_le_bytes(bmp[22..26].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(bmp[26..28].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bmp[28..30].try_into().unwrap()), 24);
        assert_eq!(u32::from_le_bytes(bmp[30..34].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(bmp[34..38].try_into().unwrap()), 24);
        // 画素データの先頭 12 バイトは下の行 (y = 1) の BGR + 0 パディング 3 バイト
        assert_eq!(
            &bmp[54..66],
            &[120, 110, 100, 150, 140, 130, 180, 170, 160, 0, 0, 0]
        );
        // 次の 12 バイトは上の行 (y = 0) の BGR + 0 パディング 3 バイト
        assert_eq!(&bmp[66..78], &[30, 20, 10, 60, 50, 40, 90, 80, 70, 0, 0, 0]);

        // decode_bmp_to_rgb で元の 3x2 RGB にそのまま戻る
        let (dw, dh, decoded) = decode_bmp_to_rgb(&bmp).unwrap();
        assert_eq!((dw, dh), (3, 2));
        assert_eq!(decoded, rgb_3x2);

        // base64 の往復
        let b64 = base64_encode(&bmp);
        assert_eq!(b64.len(), base64_encoded_len(bmp.len()));
        assert_eq!(base64_decode(&b64).unwrap(), bmp);
        for len in 0..=17 {
            let sample: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let enc = base64_encode(&sample);
            assert_eq!(base64_decode(&enc).unwrap(), sample);
        }

        // downsample_rgb と downsample_bgra_to_rgb（4x2 → scale 2 で 2x1）
        let rgb_4x2: Vec<u8> = (0..8)
            .flat_map(|i| [i * 10, i * 10 + 1, i * 10 + 2])
            .collect();
        let (sw, sh, s_rgb) = downsample_rgb(4, 2, &rgb_4x2, 2).unwrap();
        assert_eq!((sw, sh), (2, 1));
        assert_eq!(s_rgb.len(), 6);
        let bgra_4x2: Vec<u8> = (0..8)
            .flat_map(|i| [i * 10 + 2, i * 10 + 1, i * 10, 255])
            .collect();
        let (bw, bh, b_rgb) = downsample_bgra_to_rgb(4, 2, &bgra_4x2, 2).unwrap();
        assert_eq!((bw, bh), (2, 1));
        assert_eq!(b_rgb, s_rgb);

        // 2. 引数の範囲の検査（既定値、下限・上限、範囲外）
        assert_eq!(
            parse_screenshot_args(&json!({})).unwrap(),
            ScreenshotArgs {
                count: 6,
                interval_ms: 80,
                window: true,
                scale: 2,
            }
        );
        assert_eq!(
            parse_screenshot_args(&json!({
                "count": 1,
                "interval_ms": 30,
                "window": false,
                "scale": 1,
            }))
            .unwrap(),
            ScreenshotArgs {
                count: 1,
                interval_ms: 30,
                window: false,
                scale: 1,
            }
        );
        assert_eq!(
            parse_screenshot_args(&json!({
                "count": 12,
                "interval_ms": 2000,
                "window": true,
                "scale": 4,
            }))
            .unwrap(),
            ScreenshotArgs {
                count: 12,
                interval_ms: 2000,
                window: true,
                scale: 4,
            }
        );
        for bad in [
            json!({"count": 0}),
            json!({"count": 13}),
            json!({"count": -1}),
            json!({"count": "6"}),
            json!({"interval_ms": 29}),
            json!({"interval_ms": 2001}),
            json!({"interval_ms": 0}),
            json!({"window": "true"}),
            json!({"scale": 0}),
            json!({"scale": 5}),
            json!({"scale": -2}),
        ] {
            assert!(
                parse_screenshot_args(&bad).is_err(),
                "args {bad} should be rejected"
            );
        }

        // 3. Stdio でも lisa_screenshot は断られる（Peer のときだけ）
        let dir = TempDir::new("shot-auth");
        let server = Server {
            data_dir: dir.0.clone(),
            cache_dir: None,
        };
        let reply = server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "lisa_screenshot", "arguments": {}},
            }))
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("ペアリングした端末からだけ")
        );

        // 4. 合計の上限を超えるときに枚数を減らし、そのことを返事に書く
        let frames: Vec<RawShot> = (0..4)
            .map(|i| RawShot {
                taken_at: 1_000 + i * 80,
                width: 3,
                height: 2,
                window: true,
                bmp: bmp.clone(),
            })
            .collect();
        let one_b64_len = base64_encoded_len(bmp.len());
        let packed = pack_screenshots(
            ScreenshotArgs {
                count: 4,
                interval_ms: 80,
                window: true,
                scale: 2,
            },
            frames,
            one_b64_len * 2, // 2 枚分までしか入らない上限
        );
        assert_eq!(packed["count"], 2);
        assert_eq!(packed["requested_count"], 4);
        assert_eq!(packed["truncated"], true);
        assert_eq!(packed["shots"].as_array().unwrap().len(), 2);
        assert_eq!(packed["shots"][0]["taken_at"], 1_000);
        assert_eq!(packed["shots"][0]["elapsed_ms"], 0);
        assert_eq!(packed["shots"][1]["taken_at"], 1_080);
        assert_eq!(packed["shots"][1]["elapsed_ms"], 80);
        assert!(
            packed["note"]
                .as_str()
                .unwrap()
                .contains("4 枚から 2 枚に減らしました")
        );
    }
}

#[cfg(test)]
#[test]
fn render_diagnostics_env_is_allowed() {
    assert!(ALLOWED_APP_ENV_KEYS.contains(&"LISA_RENDER_DIAG"));
}
