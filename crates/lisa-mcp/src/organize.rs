//! 整理（計画の作成・状態確認・一覧・取り下げ・取り消し）を扱う MCP の道具。
//!
//! 許可する道具は作らない。計画の許可は人が Lisa の画面か CLI で行う。

use std::thread;
use std::time::{Duration, Instant};

use lisa_core::organize::{self, Context, MAX_OPS, OpInput, Plan, PlanOp, PlanState};
use lisa_core::remote::{self, Command};
use serde_json::{Value, json};

use super::{Auth, Server, arg_usize, err};

/// `lisa_plan_status` の `wait` の上限（秒）。
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
        Context::load(self.data_dir.clone(), settings.as_deref())
    }

    /// 整理の計画を確かめて作る（`dry_run: true` や `refused` があれば作らず結果だけ返す）。
    pub(super) fn plan_create(&self, args: &Value, auth: &Auth) -> Result<Value, String> {
        let ops_val = args.get("ops").ok_or("ops（操作の配列）が要ります")?;
        let ops: Vec<OpInput> = serde_json::from_value(ops_val.clone())
            .map_err(|e| format!("ops の形が正しくありません: {e}"))?;
        let note = args.get("note").and_then(Value::as_str);
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let by = match auth {
            Auth::Peer { name, .. } => format!("mcp:{name}"),
            Auth::Token => "mcp:http".to_string(),
            Auth::Stdio => "mcp:stdio".to_string(),
        };

        let mut db = self.db()?;
        let ctx = self.organize_context();
        let res = organize::create_plan(&mut db, &ctx, &by, note, &ops, dry_run)?;

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
    pub(super) fn plan_status(&self, args: &Value) -> Result<Value, String> {
        let id = require_plan_id(args)?;
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

    /// 実行済みの計画を逆の順で取り消す（許可は要らない、上書きしない）。
    pub(super) fn undo(&self, args: &Value) -> Result<Value, String> {
        let id = require_plan_id(args)?;
        let mut db = self.db()?;
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

fn require_plan_id(args: &Value) -> Result<i64, String> {
    args.get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| "id（計画の番号）が要ります".to_string())
}

fn plan_to_json(p: &Plan) -> Value {
    json!({
        "id": p.id,
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
