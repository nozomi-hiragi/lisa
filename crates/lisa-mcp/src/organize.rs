//! 整理（計画の作成・状態確認・一覧・取り下げ・取り消し）を扱う MCP の道具。
//!
//! 許可する道具は作らない。計画の許可は人が Lisa の画面か CLI で行う。

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use lisa_core::organize::{self, Context, MAX_OPS, OpInput, Plan, PlanOp, PlanState};
use lisa_core::remote::{self, Command};
use serde_json::{Value, json};

use super::{Auth, Server, arg_usize, err};

/// `lisa_plan_status` の `wait` の上限（秒）。
/// 依頼 ID の計画が対象端末にまだ無いときの理由（依頼元が「未到達」の判断に使う）。
pub const NO_PLAN_FOR_REQUEST: &str = "この依頼 ID の計画はまだありません";
const MAX_WAIT_SECS: u64 = 60;
/// `lisa_plan_status` で待つときのポーリング間隔。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// `lisa_plans` の件数の既定と上限。
const DEFAULT_PLANS_LIMIT: usize = 20;
const MAX_PLANS_LIMIT: usize = 200;
/// `lisa_plan_status` で返す操作の既定の上限。
const DEFAULT_OPS_LIMIT: usize = 200;

impl Server {
    fn organize_context(&self) -> Context {
        let settings = self.settings_path();
        let mut ctx = Context::load(self.data_dir.clone(), settings.as_deref());
        ctx.keep_client = Some(super::relay_client);
        ctx.link_client = Some(super::link_relay_client);
        ctx.relocation_client = Some(super::relocation_client);
        ctx
    }

    /// 整理の計画を確かめて作る（`dry_run: true` や `refused` があれば作らず結果だけ返す）。
    pub(super) fn plan_create(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        if args.get("remote_keep").is_some() {
            return Err("端末間の計画は lisa_duplicate_plan_create で依頼してください".into());
        }
        let ops_val = args.get("ops").ok_or("ops（操作の配列）が要ります")?;
        let ops: Vec<OpInput> = serde_json::from_value(ops_val.clone())
            .map_err(|e| format!("ops の形が正しくありません: {e}"))?;
        let note = args.get("note").and_then(Value::as_str);
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        self.create_organize_plan(auth, note, &ops, dry_run, peer_request(args, auth)?)
    }

    pub(super) fn duplicate_local_plan(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        use organize::remote_keep::{KeepReference, resolve};
        let request = peer_request(args, auth)?;
        let db = self.db()?;
        let keep: KeepReference = serde_json::from_value(args["keep"].clone()).map_err(err)?;
        let sources: Vec<KeepReference> =
            serde_json::from_value(args["sources"].clone()).map_err(err)?;
        let keep_path = resolve(&db, &keep)?;
        let mut ops = Vec::with_capacity(sources.len());
        for source in sources {
            if source.key != keep.key || source == keep {
                return Err("整理対象と残す項目が正しくありません".into());
            }
            ops.push(OpInput::Trash {
                path: resolve(&db, &source)?,
                keep: Some(keep_path.clone()),
            });
        }
        drop(db);
        self.create_organize_plan(
            auth,
            Some("重複の整理"),
            &ops,
            args["dry_run"] == true,
            request,
        )
    }

    pub(super) fn duplicate_sources(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        use organize::remote_keep::{KeepReference, ProtectedKeep};
        if !matches!(auth, Auth::Peer { .. }) {
            return Err("整理対象の確認にはペアリングが必要です".into());
        }
        let sources: Vec<KeepReference> =
            serde_json::from_value(args["sources"].clone()).map_err(err)?;
        if sources.is_empty() || sources.len() > MAX_OPS {
            return Err("整理対象の件数が範囲外です".into());
        }
        let db = self.db()?;
        let sources = sources
            .into_iter()
            .map(|source| ProtectedKeep::inspect(&db, source))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"sources": sources}))
    }

    pub(super) fn duplicate_plan_create(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        use organize::remote_keep::{RemotePlan, relay::Route};
        let Auth::Peer {
            device_id,
            name,
            remote_ip: Some(ip),
        } = auth
        else {
            return Err("端末間の整理はペアリング済みの LAN 接続が必要です".into());
        };
        let mut db = self.db()?;
        let target_device = db
            .meta("this_device")
            .map_err(err)?
            .ok_or("対象端末の ID がありません")?;
        let port = args["relay_port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or("依頼元の公開ポートが要ります")?;
        let (requester_device, request) =
            peer_request(args, auth)?.ok_or("request_id が要ります")?;
        let secret = args["relay_secret"]
            .as_str()
            .filter(|secret| secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("中継許可が要ります")?;
        let route = Route {
            requester_device: requester_device.into(),
            request_id: request.to_string(),
            target_device,
            secret: secret.into(),
            address: std::net::SocketAddr::new(*ip, port),
        };
        let keep = serde_json::from_value(args["keep"].clone()).map_err(err)?;
        let sources = serde_json::from_value(args["sources"].clone()).map_err(err)?;
        let by = format!("mcp:{name}");
        debug_assert_eq!(requester_device, device_id);
        serde_json::to_value(organize::create_remote_keep_plan(
            &mut db,
            &self.organize_context(),
            &by,
            args["note"].as_str(),
            RemotePlan {
                route,
                keep,
                sources,
                keep_label: args["keep_label"].as_str().map(|label| {
                    label
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(500)
                        .collect()
                }),
            },
        )?)
        .map_err(err)
    }

    /// 親フォルダを変えない move の計画を作る。
    pub(super) fn rename(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        let new_name = args
            .get("new_name")
            .and_then(Value::as_str)
            .ok_or("new_name が要ります")?;
        if let Some(path) = args.get("path").and_then(Value::as_str)
            && !Path::new(path).is_absolute()
        {
            return Err("path には絶対パスが必要です".into());
        }
        let db = self.db()?;
        let target = self.target(&db, args)?;
        if target.chain.len() <= 1 {
            return Err("ドライブの名前は変更できません".into());
        }
        let from = target.path;
        let to = organize::rename_destination(&from, new_name)?;
        if from == to {
            return Err("変更前と同じ名前です".into());
        }
        let default_note = format!(
            "MCPから名前変更: {} -> {new_name}",
            from.file_name().unwrap_or_default().to_string_lossy()
        );
        let note = args
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or(&default_note);
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        drop(db);
        let mut result = self.create_organize_plan(
            auth,
            Some(note),
            &[OpInput::Move { from, to }],
            dry_run,
            None,
        )?;
        if result["created"] == true {
            result["state"] = json!(PlanState::Pending);
        }
        Ok(result)
    }

    fn create_organize_plan(
        &self,
        auth: &Auth,
        note: Option<&str>,
        ops: &[OpInput],
        dry_run: bool,
        request: Option<(&str, uuid::Uuid)>,
    ) -> Result<Value, String> {
        let by = match auth {
            Auth::Peer { name, .. } => format!("mcp:{name}"),
            Auth::Token => "mcp:http".to_string(),
            Auth::Stdio => "mcp:stdio".to_string(),
        };

        let mut db = self.db()?;
        let ctx = self.organize_context();
        let res = match request {
            Some((device, request_id)) if !dry_run => {
                organize::create_peer_plan(&mut db, &ctx, &by, note, ops, device, request_id)?
            }
            Some(_) => return Err("dry_run に依頼 ID は指定できません".into()),
            None => organize::create_plan(&mut db, &ctx, &by, note, ops, dry_run)?,
        };

        let mut out = serde_json::to_value(&res).map_err(err)?;
        if res.created {
            out["note"] = json!(
                "許可は人が Lisa の画面（または CLI の lisa-scan plan approve）で押します。押されるまでファイルは何も動きません。結果は lisa_plan_status（wait 指定可）で待てます。"
            );
        } else if res.summary.refused > 0 {
            out["note"] = json!(
                "refused の操作が 1 つ以上あるため計画は作りませんでした。checks の理由を見て直してください。"
            );
        }
        Ok(out)
    }

    /// 計画の状態と操作ごとの結果を返す。`wait`（秒、最大 60）があれば許可待ちが変わるまで待つ。
    pub(super) fn plan_status(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        let id = if let Some((device, request_id)) = peer_request(args, auth)? {
            if args.get("id").is_some() {
                return Err("id と request_id は同時に指定できません".into());
            }
            organize::peer_plan_id(&self.db()?, device, request_id)
                .map_err(err)?
                .ok_or(NO_PLAN_FOR_REQUEST)?
        } else {
            require_plan_id(args)?
        };
        let wait_secs = args
            .get("wait")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(MAX_WAIT_SECS);
        let ops_offset = arg_usize(args, "ops_offset", 0);
        let ops_limit = arg_usize(args, "ops_limit", DEFAULT_OPS_LIMIT).min(MAX_OPS);

        let deadline = Instant::now() + Duration::from_secs(wait_secs);
        let (plan, ops) = loop {
            let mut db = self.db()?;
            let _ = organize::expire_stale_plans(&mut db);
            let Some((plan, ops)) = organize::get_plan(&db, id).map_err(err)? else {
                return Err(format!("計画 #{id} が見つかりません"));
            };
            drop(db);

            if plan.state.is_terminal() || Instant::now() >= deadline {
                break (plan, ops);
            }
            let remain = deadline.saturating_duration_since(Instant::now());
            thread::sleep(remain.min(WAIT_POLL_INTERVAL));
        };

        let ops_total = ops.len();
        let shown: Vec<Value> = ops
            .into_iter()
            .skip(ops_offset)
            .take(ops_limit)
            .map(op_to_json)
            .collect();

        Ok(json!({
            "plan": plan_to_json(&plan),
            "keep": organize::remote_keep::keep_summary(&self.db()?, id),
            "ops_total": ops_total,
            "ops_offset": ops_offset,
            "ops_limit": ops_limit,
            "ops": shown,
        }))
    }

    /// 計画の一覧（新しい順、状態で絞れる）。
    pub(super) fn plans(&self, args: &Value) -> Result<Value, String> {
        let state = match args.get("state").and_then(Value::as_str) {
            Some(s) => Some(PlanState::from_name(s).ok_or_else(|| {
                format!(
                    "知らない state です（pending, approved, running, done, partial, rejected, cancelled, expired, undone のどれか）: {s}"
                )
            })?),
            None => None,
        };
        let limit = arg_usize(args, "limit", DEFAULT_PLANS_LIMIT).min(MAX_PLANS_LIMIT);

        let mut db = self.db()?;
        let _ = organize::expire_stale_plans(&mut db);
        let list = organize::list_plans(&db, state, limit).map_err(err)?;
        let plans_json: Vec<Value> = list.iter().map(plan_to_json).collect();
        Ok(json!({
            "count": plans_json.len(),
            "plans": plans_json,
        }))
    }

    /// 許可待ち（`pending`）の計画を取り下げる。
    pub(super) fn plan_cancel(&self, args: &Value) -> Result<Value, String> {
        let id = require_plan_id(args)?;
        let mut db = self.db()?;
        let plan = organize::cancel_plan(&mut db, id)?;
        Ok(json!({
            "cancelled": true,
            "plan": plan_to_json(&plan),
        }))
    }

    pub(super) fn request_undo(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        let (device_id, request_id) = peer_request(args, auth)?.ok_or("request_id が要ります")?;
        let id = organize::request_peer_restore(&self.db()?, device_id, request_id)?;
        Ok(json!({"id": id, "restore_requested": true}))
    }

    /// 実行済みの計画を逆の順で取り消す（許可は要らない、上書きしない）。
    pub(super) fn undo(&self, args: &Value) -> Result<Value, String> {
        let id = require_plan_id(args)?;
        let mut db = self.db()?;
        if organize::peer_managed_plan(&db, id).map_err(err)? {
            return Err(
                "この計画の復元は対象端末の Lisa またはローカル CLI で確認してください".into(),
            );
        }
        let ctx = self.organize_context();
        let report = organize::undo_plan(&mut db, &ctx, id)?;
        for folder in &report.touched_folders {
            let _ = remote::send(
                &self.data_dir,
                Command::Rescan {
                    path: folder.clone(),
                },
            );
        }
        serde_json::to_value(&report).map_err(err)
    }
}

fn peer_request<'a>(args: &Value, auth: &'a Auth) -> Result<Option<(&'a str, uuid::Uuid)>, String> {
    let Some(value) = args.get("request_id") else {
        return Ok(None);
    };
    let Auth::Peer { device_id, .. } = auth else {
        return Err("依頼 ID はペアリング済みの端末からだけ使えます".into());
    };
    let id = value
        .as_str()
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .ok_or("request_id は UUID で指定してください")?;
    Ok(Some((device_id, id)))
}

fn require_plan_id(args: &Value) -> Result<i64, String> {
    args.get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| "id（計画の番号）が要ります".to_string())
}

fn plan_to_json(p: &Plan) -> Value {
    json!({
        "id": p.id,
        "restore_requested": p.restore_requested,
        "state": p.state.as_str(),
        "by": p.by,
        "note": p.note,
        "summary": p.summary,
        "skip_risky": p.skip_risky,
        "created_at": p.created_at,
        "decided_at": p.decided_at,
        "finished_at": p.finished_at,
        "purged_at": p.purged_at,
    })
}

fn op_to_json(o: PlanOp) -> Value {
    json!({
        "seq": o.seq,
        "op": o.op.as_str(),
        "drive_id": o.drive_id,
        "src": o.src,
        "dst": o.dst,
        "keep": o.keep,
        "size": o.size,
        "mtime": o.mtime,
        "hash": o.hash.map(|h| lisa_core::query::hex(&h)),
        "risky": o.risky,
        "state": o.state.as_str(),
        "reason": o.reason,
        "done_at": o.done_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use lisa_core::db::Db;
    use lisa_core::hash::{self, Pace};
    use lisa_core::listing::{Kind, read_dir};
    use lisa_core::volumes::Volume;

    struct Env {
        root: PathBuf,
        drive: PathBuf,
        data_dir: PathBuf,
        server: Server,
    }

    impl Env {
        fn new(name: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "lisa-mcp-org-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            let drive = root.join("drive");
            let data_dir = root.join("data");
            fs::create_dir_all(&drive).unwrap();
            fs::create_dir_all(&data_dir).unwrap();
            let mut db = Db::open(&data_dir.join("lisa.db")).unwrap();
            let dev = db.this_device("mac", "mac.local", "macos").unwrap();
            db.upsert_drive(
                &dev,
                &Volume {
                    id: Some("test:mcp".into()),
                    label: "T".into(),
                    fs: "apfs".into(),
                    mount_path: drive.clone(),
                    capacity: None,
                    available: None,
                    removable: false,
                    network: false,
                    read_only: false,
                    case_sensitive: None,
                    hidden: false,
                },
            )
            .unwrap();
            let server = Server {
                data_dir: data_dir.clone(),
                cache_dir: None,
            };
            Self {
                root,
                drive,
                data_dir,
                server,
            }
        }

        fn scan(&self) {
            let mut db = Db::open(&self.data_dir.join("lisa.db")).unwrap();
            fn walk(db: &mut Db, mount: &Path, dir: &Path) {
                let res = read_dir(dir).map_err(|e| e.to_string());
                db.save_listing("test:mcp", mount, dir, &res).unwrap();
                if let Ok(listing) = res {
                    for e in listing.entries {
                        if e.kind == Kind::Dir {
                            walk(db, mount, &dir.join(&e.name));
                        }
                    }
                }
            }
            walk(&mut db, &self.drive, &self.drive);
            hash::hash_drive(
                &mut db,
                "test:mcp",
                &self.drive,
                2,
                &mut |_| {},
                &Pace::FREE,
            )
            .unwrap();
        }

        fn call(&self, name: &str, args: Value) -> Value {
            self.server
                .call(&json!({"name": name, "arguments": args}), &Auth::Stdio)
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn mcp_tools_list_has_organize_tools_and_no_approve_tool() {
        let list = crate::tools();
        let names: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        for required in [
            "lisa_plan_create",
            "lisa_rename",
            "lisa_plan_status",
            "lisa_plans",
            "lisa_plan_cancel",
            "lisa_undo",
        ] {
            assert!(names.contains(&required), "missing tool: {required}");
        }
        assert!(
            !names.iter().any(|n| n.contains("approve")),
            "approve tool must not exist in MCP"
        );
    }

    #[test]
    fn mcp_rename_validates_and_creates_a_pending_plan() {
        let env = Env::new("rename");
        let path = env.drive.join("old.txt");
        fs::write(&path, b"old").unwrap();
        fs::write(env.drive.join("taken.txt"), b"taken").unwrap();
        env.scan();
        for name in ["", ".", "..", "a/b", "a\\b", "old.txt"] {
            let result = env.call("lisa_rename", json!({"path": path, "new_name": name}));
            assert_eq!(result["isError"], true, "{result}");
        }
        let collision = env.call(
            "lisa_rename",
            json!({"path": path, "new_name": "taken.txt"}),
        );
        assert_eq!(
            collision["structuredContent"]["created"], false,
            "{collision}"
        );
        assert_eq!(collision["structuredContent"]["summary"]["refused"], 1);
        let db = env.server.db().unwrap();
        let id = *lisa_core::query::locate(&db, &path)
            .unwrap()
            .unwrap()
            .chain
            .last()
            .unwrap();
        drop(db);
        let dry = env.call(
            "lisa_rename",
            json!({"id": id, "new_name": "new.txt", "dry_run": true}),
        );
        assert_eq!(dry["structuredContent"]["created"], false, "{dry}");
        assert_eq!(dry["structuredContent"]["dry_run"], true);
        assert_eq!(dry["structuredContent"]["summary"]["ok"], 1);
        assert_eq!(dry["structuredContent"]["id"], Value::Null);
        let result = env.call("lisa_rename", json!({"path": path, "new_name": "new.txt"}));
        let plan = &result["structuredContent"];
        assert_eq!(plan["created"], true, "{result}");
        assert!(plan["id"].is_i64());
        assert_eq!(plan["state"], "pending");
        let renamed = env.drive.join("new.txt");
        assert!(path.exists());
        assert!(!renamed.exists());

        let plan_id = plan["id"].as_i64().unwrap();
        let mut db = env.server.db().unwrap();
        let ctx = env.server.organize_context();
        let (pending, _) = organize::get_plan(&db, plan_id).unwrap().unwrap();
        assert_eq!(pending.state, PlanState::Pending);
        assert_eq!(
            pending.note.as_deref(),
            Some("MCPから名前変更: old.txt -> new.txt")
        );
        organize::approve_plan(&mut db, plan_id, false).unwrap();
        let applied = organize::execute_plan(&mut db, &ctx, plan_id).unwrap();
        assert_eq!(applied.done, 1);
        assert_eq!(applied.touched_folders, vec![env.drive.clone()]);
        assert!(!path.exists());
        assert_eq!(fs::read(&renamed).unwrap(), b"old");
        let undone = organize::undo_plan(&mut db, &ctx, plan_id).unwrap();
        assert_eq!(undone.state, PlanState::Undone);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(!renamed.exists());
    }

    #[test]
    fn peer_request_id_is_durable_and_scoped_to_the_paired_device() {
        let env = Env::new("peer-request");
        let kept = env.drive.join("keep.txt");
        let copy = env.drive.join("copy.txt");
        for path in [&kept, &copy] {
            fs::write(path, b"same").unwrap();
        }
        env.scan();
        let request_id = uuid::Uuid::new_v4().to_string();
        let peer = |device_id: &str| Auth::Peer {
            device_id: device_id.into(),
            name: "旧PC".into(),
            remote_ip: None,
        };
        let id = json!({"request_id": request_id});
        let request = json!({
            "ops": [{"op":"trash", "path":copy, "keep":kept}],
            "request_id": request_id,
        });
        let call = |name, args, auth: &Auth| {
            env.server
                .call(&json!({"name":name,"arguments":args}), auth)
        };
        let auth = peer("requester");
        assert_eq!(
            call("lisa_plan_create", request.clone(), &Auth::Stdio)["isError"],
            true
        );
        let barrier = std::sync::Barrier::new(2);
        let create = || {
            barrier.wait();
            call("lisa_plan_create", request.clone(), &auth)
        };
        let (first, retry) = std::thread::scope(|scope| {
            let first = scope.spawn(create);
            let retry = create();
            (first.join().unwrap(), retry)
        });
        assert_eq!(first, retry);
        let plan_id = first["structuredContent"]["id"]
            .as_i64()
            .expect("計画ができた");
        let status = || call("lisa_plan_status", id.clone(), &auth);
        assert_eq!(status()["structuredContent"]["plan"]["state"], "pending");
        assert_eq!(
            call("lisa_plan_status", id.clone(), &peer("other"))["isError"],
            true
        );
        let mut changed = request.clone();
        changed["note"] = json!("別の依頼");
        assert_eq!(call("lisa_plan_create", changed, &auth)["isError"], true);
        assert!(copy.is_file() && kept.is_file());

        let mtime = fs::metadata(&copy).unwrap().modified().unwrap();
        let mut db = env.server.db().unwrap();
        assert_eq!(organize::list_plans(&db, None, 10).unwrap().len(), 1);
        let ctx = env.server.organize_context();
        organize::approve_plan(&mut db, plan_id, false).unwrap();
        let result = organize::execute_plan(&mut db, &ctx, plan_id).unwrap();
        assert_eq!(result.done, 1);
        assert!(!copy.exists());
        let saved = organize::plan_ops(&db, plan_id).unwrap();
        let isolated = saved[0].dst.as_ref().unwrap();
        assert_eq!(fs::metadata(isolated).unwrap().modified().unwrap(), mtime);
        drop(db);
        assert_eq!(status()["structuredContent"]["plan"]["state"], "done");
        assert_eq!(call("lisa_plan_create", request, &auth), first);
        let mut db = env.server.db().unwrap();
        assert!(organize::execute_plan(&mut db, &ctx, plan_id).is_err());
        assert_eq!(
            env.call("lisa_undo", json!({"id": plan_id}))["isError"],
            true
        );
        assert_eq!(
            call("lisa_plan_request_undo", id.clone(), &peer("other"))["isError"],
            true
        );
        for _ in 0..2 {
            assert_eq!(
                call("lisa_plan_request_undo", id.clone(), &auth)["structuredContent"]["id"],
                plan_id
            );
        }
        assert_eq!(
            status()["structuredContent"]["plan"]["restore_requested"],
            true
        );
        assert!(!copy.exists());
        fs::write(&copy, b"occupied").unwrap();
        let undo = organize::undo_plan(&mut db, &ctx, plan_id).unwrap();
        assert_eq!(undo.skipped.len(), 1);
        assert_eq!(fs::read(&copy).unwrap(), b"occupied");
        assert_eq!(fs::read(isolated).unwrap(), b"same");
        assert_eq!(
            status()["structuredContent"]["plan"]["restore_requested"],
            true
        );
        fs::remove_file(&copy).unwrap();
        let restored = organize::undo_plan(&mut db, &ctx, plan_id).unwrap();
        assert_eq!(restored.undone, 1);
        assert_eq!(fs::read(&copy).unwrap(), b"same");
        assert_eq!(fs::metadata(copy).unwrap().modified().unwrap(), mtime);
        assert_eq!(
            status()["structuredContent"]["plan"]["restore_requested"],
            false
        );
        assert_eq!(call("lisa_plan_request_undo", id, &auth)["isError"], true);
    }

    #[test]
    fn mcp_plan_create_status_wait_cancel_and_undo() {
        let env = Env::new("flow");
        let a = env.drive.join("a.txt");
        let b = env.drive.join("b.txt");
        let moved = env.drive.join("a-moved.txt");
        fs::write(&a, b"same").unwrap();
        fs::write(&b, b"same").unwrap();
        env.scan();

        // 1. dry_run では作られない。
        let dry = env.call(
            "lisa_plan_create",
            json!({
                "ops": [{"op": "move", "from": a, "to": moved}],
                "note": "下見",
                "dry_run": true,
            }),
        );
        assert_ne!(dry.get("isError"), Some(&json!(true)), "{dry}");
        let sc = &dry["structuredContent"];
        assert_eq!(sc["created"], json!(false));
        assert_eq!(sc["dry_run"], json!(true));
        assert_eq!(sc["id"], Value::Null);

        // 2. 本番作成 → pending。
        let created = env.call(
            "lisa_plan_create",
            json!({
                "ops": [
                    {"op": "move", "from": a, "to": moved},
                    {"op": "trash", "path": b, "keep": moved},
                ],
                "note": "整理",
            }),
        );
        // 注: b の keep は移動前の moved（まだ無い）ので risky になるが計画は作れる。
        assert_ne!(created.get("isError"), Some(&json!(true)), "{created}");
        let id = created["structuredContent"]["id"].as_i64().unwrap();
        assert!(a.exists() && !moved.exists());

        // 3. lisa_plans に出る。
        let listed = env.call("lisa_plans", json!({"state": "pending"}));
        assert_eq!(listed["structuredContent"]["count"], json!(1));

        // 4. 別スレッドで許可＆実行しつつ、lisa_plan_status の wait で待つ。
        let data_dir = env.data_dir.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            let mut db = Db::open(&data_dir.join("lisa.db")).unwrap();
            let ctx = Context::with_builtin(data_dir);
            organize::approve_plan(&mut db, id, false).unwrap();
            organize::execute_plan(&mut db, &ctx, id).unwrap();
        });
        let status = env.call("lisa_plan_status", json!({"id": id, "wait": 5}));
        handle.join().unwrap();
        assert_ne!(status.get("isError"), Some(&json!(true)), "{status}");
        assert_eq!(status["structuredContent"]["plan"]["state"], json!("done"));
        assert!(!a.exists() && moved.exists() && !b.exists());

        // 5. lisa_undo で戻る。
        let undone = env.call("lisa_undo", json!({"id": id}));
        assert_ne!(undone.get("isError"), Some(&json!(true)), "{undone}");
        assert_eq!(undone["structuredContent"]["state"], json!("undone"));
        assert_eq!(undone["structuredContent"]["undone"], json!(2));
        assert!(a.exists() && !moved.exists() && b.exists());

        // 6. 新しい計画を作って lisa_plan_cancel で取り下げ。
        let c2 = env.call(
            "lisa_plan_create",
            json!({"ops": [{"op": "move", "from": a, "to": moved}]}),
        );
        let id2 = c2["structuredContent"]["id"].as_i64().unwrap();
        let cancelled = env.call("lisa_plan_cancel", json!({"id": id2}));
        assert_ne!(cancelled.get("isError"), Some(&json!(true)), "{cancelled}");
        assert_eq!(
            cancelled["structuredContent"]["plan"]["state"],
            json!("cancelled")
        );
    }
}
