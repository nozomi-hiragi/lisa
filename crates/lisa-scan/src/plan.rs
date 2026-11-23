//! 整理の計画を扱う CLI（`lisa-scan plan ...`）。
//!
//! ```text
//! lisa-scan plan create <ops.json> [--note <説明>] [--dry-run]
//! lisa-scan plan list [--state <状態>] [--limit N]
//! lisa-scan plan show <id>
//! lisa-scan plan approve <id> [--skip-risky]
//! lisa-scan plan reject <id>
//! lisa-scan plan undo <id>
//! lisa-scan plan purge [--days N]
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};

use lisa_core::db::Db;
use lisa_core::organize::{self, Context, DEFAULT_TRASH_DAYS, OpInput, OpKind, PlanState};
use lisa_core::remote::{self, Command};
use serde_json::Value;

use crate::{app_data_dir, app_db, human, settings_path};

const USAGE_PLAN: &str = "使い方:
  lisa-scan plan create <ops.json> [--note <説明>] [--dry-run]  操作の JSON から計画を作る（- で標準入力）
  lisa-scan plan list [--state <状態>] [--limit N]              計画の一覧（新しい順）
  lisa-scan plan show <id>                                      計画の中身と操作ごとの結果
  lisa-scan plan approve <id> [--skip-risky]                    計画を許可して実行する
  lisa-scan plan reject <id>                                    許可待ちの計画を断る
  lisa-scan plan undo <id>                                      実行済みの計画を取り消す
  lisa-scan plan purge [--days N]                               期限切れの隔離フォルダを OS のゴミ箱へ送る";

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanCmd {
    Create {
        file: PathBuf,
        note: Option<String>,
        dry_run: bool,
    },
    List {
        state: Option<PlanState>,
        limit: usize,
    },
    Show {
        id: i64,
    },
    Approve {
        id: i64,
        skip_risky: bool,
    },
    Reject {
        id: i64,
    },
    Undo {
        id: i64,
    },
    Purge {
        days: Option<u64>,
    },
}

pub fn run(args: &[&str]) -> Result<(), String> {
    let cmd = parse_args(args)?;
    let data_dir = app_data_dir()?;
    let settings = settings_path();
    let mut ctx = Context::load(data_dir.clone(), settings.as_deref());
    ctx.keep_client = Some(lisa_mcp::relay_client);
    let mut db = app_db()?;
    run_with(&mut db, &ctx, settings.as_deref(), cmd)
}

fn run_with(
    db: &mut Db,
    ctx: &Context,
    settings_file: Option<&Path>,
    cmd: PlanCmd,
) -> Result<(), String> {
    match cmd {
        PlanCmd::Create {
            file,
            note,
            dry_run,
        } => {
            let text = read_ops_text(&file)?;
            let (ops, json_note) = parse_ops_json(&text)?;
            let note = note.or(json_note);
            let res = organize::create_plan(db, ctx, "cli", note.as_deref(), &ops, dry_run)?;
            print_create_result(&res);
            if res.summary.refused > 0 {
                return Err(format!(
                    "断られた操作が {} 件あるため計画を作れませんでした",
                    res.summary.refused
                ));
            }
            Ok(())
        }
        PlanCmd::List { state, limit } => {
            let _ = organize::expire_stale_plans(db);
            let plans = organize::list_plans(db, state, limit).map_err(|e| e.to_string())?;
            if plans.is_empty() {
                println!("計画はありません。");
                return Ok(());
            }
            for p in &plans {
                let note = p.note.as_deref().unwrap_or("");
                println!(
                    "#{:<4}  {:<9}  {:<12}  {} 件（危ない {} / 移動 {} / 空く {}）  {}",
                    p.id,
                    p.state.as_str(),
                    p.by,
                    p.summary.total,
                    p.summary.risky,
                    human(p.summary.move_bytes),
                    human(p.summary.free_bytes),
                    note
                );
            }
            println!("--\n{} 件", plans.len());
            Ok(())
        }
        PlanCmd::Show { id } => {
            let _ = organize::expire_stale_plans(db);
            let Some((plan, ops)) = organize::get_plan(db, id).map_err(|e| e.to_string())? else {
                return Err(format!("計画 #{id} が見つかりません"));
            };
            println!(
                "計画 #{}  状態: {}  作成: {}  作成時刻: {} ms",
                plan.id,
                plan.state.as_str(),
                plan.by,
                plan.created_at
            );
            if let Some(note) = &plan.note {
                println!("説明: {note}");
            }
            println!(
                "要約: 全 {} 件（mkdir {} / move {} / trash {}、危ない {}、移動 {}、空く {}）",
                plan.summary.total,
                plan.summary.mkdir,
                plan.summary.move_ops,
                plan.summary.trash,
                plan.summary.risky,
                human(plan.summary.move_bytes),
                human(plan.summary.free_bytes),
            );
            if plan.skip_risky {
                println!("危ないものを除いて許可: はい");
            }
            if let Some(t) = plan.purged_at {
                println!("ゴミ箱へ送り済み: {t} ms");
            }
            if let Some(keep) = organize::remote_keep::keep_summary(db, id) {
                println!("{}", keep.line());
                println!("{}", keep.note());
            }
            println!("--");
            for op in &ops {
                let size = op.size.map(human).unwrap_or_default();
                let target = match op.op {
                    OpKind::Move => format!(
                        "{} -> {}",
                        op.src.display(),
                        op.dst
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ),
                    OpKind::Trash => match &op.dst {
                        Some(q) => format!("{} (隔離: {})", op.src.display(), q.display()),
                        None => op.src.display().to_string(),
                    },
                    OpKind::Mkdir => op.src.display().to_string(),
                };
                println!(
                    "  #{:<3} {:<5} {:<7} {:>9}  {}",
                    op.seq,
                    op.op.as_str(),
                    op.state.as_str(),
                    size,
                    target
                );
                if let Some(risky) = &op.risky {
                    println!("        危ない: {risky}");
                }
                if let Some(reason) = &op.reason {
                    println!("        理由: {reason}");
                }
            }
            Ok(())
        }
        PlanCmd::Approve { id, skip_risky } => {
            organize::approve_plan_with_context(db, ctx, id, skip_risky)?;
            let report = organize::execute_plan(db, ctx, id)?;
            notify_rescan(&ctx.data_dir, &report.touched_folders);
            println!(
                "計画 #{} を実行しました（状態: {}、完了 {} / スキップ {} / 失敗 {}）",
                report.plan_id,
                report.state.as_str(),
                report.done,
                report.skipped,
                report.failed
            );
            if let Ok(Some((_, ops))) = organize::get_plan(db, id) {
                for op in ops {
                    if let Some(reason) = &op.reason {
                        println!(
                            "  #{:<3} {:<7} {}  ({})",
                            op.seq,
                            op.state.as_str(),
                            op.src.display(),
                            reason
                        );
                    }
                }
            }
            Ok(())
        }
        PlanCmd::Reject { id } => {
            let plan = organize::reject_plan(db, id)?;
            println!(
                "計画 #{} を断りました（状態: {}）",
                plan.id,
                plan.state.as_str()
            );
            Ok(())
        }
        PlanCmd::Undo { id } => {
            let report = organize::undo_plan(db, ctx, id)?;
            notify_rescan(&ctx.data_dir, &report.touched_folders);
            println!(
                "計画 #{} を取り消しました（状態: {}、戻した {} 件 / 飛ばした {} 件）",
                report.plan_id,
                report.state.as_str(),
                report.undone,
                report.skipped.len()
            );
            for skip in &report.skipped {
                println!(
                    "  #{:<3} 飛ばした  {}  ({})",
                    skip.seq,
                    skip.src.display(),
                    skip.reason
                );
            }
            Ok(())
        }
        PlanCmd::Purge { days } => {
            let days = days.unwrap_or_else(|| trash_days_from_settings(settings_file));
            let report = organize::purge_expired(db, &ctx.data_dir, days)?;
            println!(
                "{days} 日以上前の隔離フォルダを OS のゴミ箱へ送りました（計画 {} 件、フォルダ {} 個）",
                report.purged_plans.len(),
                report.trashed_dirs.len()
            );
            for dir in &report.trashed_dirs {
                println!("  -> {}", dir.display());
            }
            for (pid, err) in &report.errors {
                eprintln!("  計画 #{pid} で失敗: {err}");
            }
            if !report.errors.is_empty() {
                return Err(format!("{} 件の掃除に失敗しました", report.errors.len()));
            }
            Ok(())
        }
    }
}

fn notify_rescan(data_dir: &Path, folders: &[PathBuf]) {
    for folder in folders {
        let _ = remote::send(
            data_dir,
            Command::Rescan {
                path: folder.clone(),
            },
        );
    }
}

fn trash_days_from_settings(settings_file: Option<&Path>) -> u64 {
    let Some(path) = settings_file else {
        return DEFAULT_TRASH_DAYS;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return DEFAULT_TRASH_DAYS;
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("trash_days").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_TRASH_DAYS)
}

fn read_ops_text(file: &Path) -> Result<String, String> {
    if file == Path::new("-") {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("標準入力を読めませんでした: {e}"))?;
        Ok(s)
    } else {
        std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))
    }
}

/// 配列 `[...]` またはオブジェクト `{"ops": [...], "note": "..."}` を読む。
fn parse_ops_json(text: &str) -> Result<(Vec<OpInput>, Option<String>), String> {
    let val: Value = serde_json::from_str(text).map_err(|e| format!("JSON が読めません: {e}"))?;
    match val {
        Value::Array(_) => {
            let ops: Vec<OpInput> =
                serde_json::from_value(val).map_err(|e| format!("操作の配列が読めません: {e}"))?;
            Ok((ops, None))
        }
        Value::Object(mut obj) => {
            let note = obj.get("note").and_then(Value::as_str).map(str::to_string);
            let ops_val = obj
                .remove("ops")
                .ok_or_else(|| "JSON オブジェクトに ops 配列がありません".to_string())?;
            let ops: Vec<OpInput> = serde_json::from_value(ops_val)
                .map_err(|e| format!("ops の配列が読めません: {e}"))?;
            Ok((ops, note))
        }
        _ => Err("JSON は操作の配列 [...] か {\"ops\": [...]} で書いてください".into()),
    }
}

fn print_create_result(res: &organize::CreateResult) {
    if let Some(id) = res.id {
        println!("計画 #{id} を作りました（許可待ち。実行するには lisa-scan plan approve {id}）。");
    } else if res.dry_run {
        println!("ドライランの結果（計画は作っていません）:");
    } else {
        println!("計画を作れませんでした:");
    }
    println!(
        "  全 {} 件（mkdir {} / move {} / trash {}、ok {} / risky {} / refused {}、移動 {} / 空く {}）",
        res.summary.total,
        res.summary.mkdir,
        res.summary.move_ops,
        res.summary.trash,
        res.summary.ok,
        res.summary.risky,
        res.summary.refused,
        human(res.summary.move_bytes),
        human(res.summary.free_bytes),
    );
    for c in &res.checks {
        if c.check != "ok" || res.dry_run {
            let target = match &c.dst {
                Some(dst) => format!("{} -> {}", c.src.display(), dst.display()),
                None => c.src.display().to_string(),
            };
            println!(
                "  #{:<3} {:<5} {:<24} {}",
                c.seq,
                c.op.as_str(),
                c.check,
                target
            );
        }
    }
}

fn parse_args(args: &[&str]) -> Result<PlanCmd, String> {
    let Some((&sub, rest)) = args.split_first() else {
        return Err(USAGE_PLAN.to_string());
    };
    match sub {
        "create" => parse_create(rest),
        "list" => parse_list(rest),
        "show" => parse_id_only("show", rest).map(|id| PlanCmd::Show { id }),
        "approve" => parse_approve(rest),
        "reject" => parse_id_only("reject", rest).map(|id| PlanCmd::Reject { id }),
        "undo" => parse_id_only("undo", rest).map(|id| PlanCmd::Undo { id }),
        "purge" => parse_purge(rest),
        _ => Err(format!("知らない plan のサブコマンド: {sub}\n{USAGE_PLAN}")),
    }
}

fn parse_create(args: &[&str]) -> Result<PlanCmd, String> {
    const USAGE: &str = "使い方: lisa-scan plan create <ops.json> [--note <説明>] [--dry-run]";
    let mut file: Option<PathBuf> = None;
    let mut note: Option<String> = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--dry-run" {
            dry_run = true;
            i += 1;
        } else if arg == "--note" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--note に説明が要ります\n{USAGE}"));
            };
            note = Some(val.to_string());
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--note=") {
            note = Some(val.to_string());
            i += 1;
        } else if arg != "-" && arg.starts_with('-') {
            return Err(format!("知らないオプション: {arg}\n{USAGE}"));
        } else if file.is_none() {
            file = Some(PathBuf::from(arg));
            i += 1;
        } else {
            return Err(USAGE.to_string());
        }
    }
    let Some(file) = file else {
        return Err(USAGE.to_string());
    };
    Ok(PlanCmd::Create {
        file,
        note,
        dry_run,
    })
}

fn parse_list(args: &[&str]) -> Result<PlanCmd, String> {
    const USAGE: &str = "使い方: lisa-scan plan list [--state <状態>] [--limit N]";
    let mut state: Option<PlanState> = None;
    let mut limit: usize = 30;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--state" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--state に状態が要ります\n{USAGE}"));
            };
            state = Some(PlanState::from_name(val).ok_or_else(|| format!("知らない状態: {val}"))?);
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--state=") {
            state = Some(PlanState::from_name(val).ok_or_else(|| format!("知らない状態: {val}"))?);
            i += 1;
        } else if arg == "--limit" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--limit に件数が要ります\n{USAGE}"));
            };
            limit = val.parse().map_err(|_| format!("--limit は数で: {val}"))?;
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--limit=") {
            limit = val.parse().map_err(|_| format!("--limit は数で: {val}"))?;
            i += 1;
        } else {
            return Err(format!("知らない引数: {arg}\n{USAGE}"));
        }
    }
    Ok(PlanCmd::List { state, limit })
}

fn parse_id_only(sub: &str, args: &[&str]) -> Result<i64, String> {
    match args {
        [id_str] => id_str
            .parse::<i64>()
            .map_err(|_| format!("計画の ID は数で: {id_str}")),
        _ => Err(format!("使い方: lisa-scan plan {sub} <id>")),
    }
}

fn parse_approve(args: &[&str]) -> Result<PlanCmd, String> {
    const USAGE: &str = "使い方: lisa-scan plan approve <id> [--skip-risky]";
    let mut id: Option<i64> = None;
    let mut skip_risky = false;
    for &arg in args {
        if arg == "--skip-risky" {
            skip_risky = true;
        } else if arg.starts_with('-') {
            return Err(format!("知らないオプション: {arg}\n{USAGE}"));
        } else if id.is_none() {
            id = Some(
                arg.parse::<i64>()
                    .map_err(|_| format!("計画の ID は数で: {arg}"))?,
            );
        } else {
            return Err(USAGE.to_string());
        }
    }
    let Some(id) = id else {
        return Err(USAGE.to_string());
    };
    Ok(PlanCmd::Approve { id, skip_risky })
}

fn parse_purge(args: &[&str]) -> Result<PlanCmd, String> {
    const USAGE: &str = "使い方: lisa-scan plan purge [--days N]";
    let mut days: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--days" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--days に日数が要ります\n{USAGE}"));
            };
            days = Some(val.parse().map_err(|_| format!("--days は数で: {val}"))?);
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--days=") {
            days = Some(val.parse().map_err(|_| format!("--days は数で: {val}"))?);
            i += 1;
        } else {
            return Err(format!("知らない引数: {arg}\n{USAGE}"));
        }
    }
    Ok(PlanCmd::Purge { days })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn parse_plan_subcommands() {
        assert_eq!(
            parse_args(&["create", "ops.json", "--note", "掃除", "--dry-run"]).unwrap(),
            PlanCmd::Create {
                file: PathBuf::from("ops.json"),
                note: Some("掃除".into()),
                dry_run: true,
            }
        );
        assert_eq!(
            parse_args(&["list", "--state=pending", "--limit", "5"]).unwrap(),
            PlanCmd::List {
                state: Some(PlanState::Pending),
                limit: 5,
            }
        );
        assert_eq!(
            parse_args(&["show", "12"]).unwrap(),
            PlanCmd::Show { id: 12 }
        );
        assert_eq!(
            parse_args(&["approve", "7", "--skip-risky"]).unwrap(),
            PlanCmd::Approve {
                id: 7,
                skip_risky: true,
            }
        );
        assert_eq!(
            parse_args(&["reject", "3"]).unwrap(),
            PlanCmd::Reject { id: 3 }
        );
        assert_eq!(parse_args(&["undo", "4"]).unwrap(), PlanCmd::Undo { id: 4 });
        assert_eq!(
            parse_args(&["purge", "--days", "14"]).unwrap(),
            PlanCmd::Purge { days: Some(14) }
        );
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["approve"]).is_err());
    }

    #[test]
    fn cli_plan_create_approve_undo_end_to_end() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "lisa-scan-plan-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let drive = root.join("drive");
        let data_dir = root.join("data");
        fs::create_dir_all(&drive).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let src = drive.join("hello.txt");
        let dst = drive.join("moved.txt");
        fs::write(&src, b"cli test").unwrap();

        let mut db = Db::open(&data_dir.join("lisa.db")).unwrap();
        let dev = db.this_device("mac", "mac", "macos").unwrap();
        db.upsert_drive(
            &dev,
            &lisa_core::volumes::Volume {
                id: Some("test:cli".into()),
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
        let listing = lisa_core::listing::read_dir(&drive).map_err(|e| e.to_string());
        db.save_listing("test:cli", &drive, &drive, &listing)
            .unwrap();

        let ops_file = root.join("ops.json");
        let json = serde_json::json!([
            {"op": "move", "from": src, "to": dst}
        ]);
        fs::write(&ops_file, serde_json::to_string(&json).unwrap()).unwrap();

        let mut ctx = Context::with_builtin(data_dir);
        ctx.home_dir = Some(root.join("fake-home"));

        run_with(
            &mut db,
            &ctx,
            None,
            PlanCmd::Create {
                file: ops_file,
                note: Some("テスト".into()),
                dry_run: false,
            },
        )
        .unwrap();
        let plans = organize::list_plans(&db, None, 10).unwrap();
        assert_eq!(plans.len(), 1);
        let id = plans[0].id;
        assert_eq!(plans[0].state, PlanState::Pending);

        run_with(
            &mut db,
            &ctx,
            None,
            PlanCmd::Approve {
                id,
                skip_risky: false,
            },
        )
        .unwrap();
        assert!(!src.exists() && dst.exists());

        run_with(&mut db, &ctx, None, PlanCmd::Undo { id }).unwrap();
        assert!(src.exists() && !dst.exists());

        let _ = fs::remove_dir_all(&root);
    }
}
