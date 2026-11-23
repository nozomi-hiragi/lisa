use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Barrier, Mutex},
};

use lisa_core::{
    db::Db,
    dups::{COMPARISON_FORMAT, ContentKey},
    hash::{self, Pace},
    listing,
    organize::{
        self, Context, PlanState,
        remote_keep::{
            KeepReference,
            relay::{RelayAction, Route},
        },
    },
    volumes::Volume,
};
use serde_json::{Value, json};

use super::{Server, peer};
use crate::{Auth, HttpListener, ListenerHandle, relay_client};

const AB: &str = "fixture-a-to-b";
const BA: &str = "fixture-b-to-a";

struct Device {
    root: PathBuf,
    mount: PathBuf,
    dir: PathBuf,
    id: String,
    port: u16,
    handle: ListenerHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Device {
    fn create(root: &Path, label: &str) -> Self {
        let root = root.join(label);
        let dir = root.join("data");
        let mount = root.join("volume");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&mount).unwrap();
        for n in 0..3 {
            fs::write(mount.join(format!("{n}.txt")), b"same").unwrap();
        }
        fs::create_dir_all(mount.join("folder")).unwrap();
        fs::write(mount.join("folder/x.txt"), b"same").unwrap();
        let mut db = Db::open(&dir.join("lisa.db")).unwrap();
        let id = db.this_device(label, label, "fixture").unwrap();
        db.upsert_drive(
            &id,
            &Volume {
                id: Some("fixture-drive".into()),
                label: "fixture".into(),
                fs: "fixture".into(),
                mount_path: mount.clone(),
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
        db.save_listing(
            "fixture-drive",
            &mount,
            &mount,
            &listing::read_dir(&mount).map_err(|e| e.to_string()),
        )
        .unwrap();
        db.save_listing(
            "fixture-drive",
            &mount,
            &mount.join("folder"),
            &listing::read_dir(&mount.join("folder")).map_err(|error| error.to_string()),
        )
        .unwrap();
        hash::hash_drive(
            &mut db,
            "fixture-drive",
            &mount,
            1,
            &mut |_| {},
            &Pace::FREE,
        )
        .unwrap();
        drop(db);
        let server = Server {
            data_dir: dir.clone(),
            cache_dir: None,
        };
        let listener = HttpListener::bind("127.0.0.1:0", server, "synthetic-token".into()).unwrap();
        let port = listener.local_addr().port();
        let handle = listener.handle();
        let thread = Some(std::thread::spawn(move || listener.run()));
        Self {
            root,
            mount,
            dir,
            id,
            port,
            handle,
            thread,
        }
    }

    fn pair(&self, other: &Device, send: &str, accept: &str) {
        fs::write(
            self.dir.join("peers.json"),
            json!([{
                "device_id":other.id,"addr":"127.0.0.1","port":other.port,"key":send
            }])
            .to_string(),
        )
        .unwrap();
        fs::write(
            self.dir.join("lan.json"),
            json!({"peers":[{
                "device_id":other.id,"name":"fixture","key":accept,"added_at":0
            }]})
            .to_string(),
        )
        .unwrap();
    }

    fn reopen(&self) -> Db {
        Db::open(&self.dir.join("lisa.db")).unwrap()
    }

    fn reference(&self, filename: &str) -> KeepReference {
        let db = self.reopen();
        let location = lisa_core::query::locate(&db, &self.mount.join(filename))
            .unwrap()
            .unwrap();
        let id = location.chain.last().copied().unwrap();
        let item = db.entry(id).unwrap().unwrap();
        let key = ContentKey {
            directory: item.kind == listing::Kind::Dir,
            size: if item.kind == listing::Kind::Dir {
                4
            } else {
                item.size
            },
            hash: item.hash.unwrap(),
        };
        organize::remote_keep::ProtectedKeep::inspect(
            &db,
            KeepReference {
                device_id: self.id.clone(),
                drive_id: location.drive_id,
                chain: location.chain,
                format: COMPARISON_FORMAT,
                key,
                identity: [0; 2],
            },
        )
        .unwrap()
    }

    fn ask(&self, target: &Device, files: &[&str]) -> i64 {
        self.ask_keep(target, "0.txt", files)
    }

    fn ask_keep(&self, target: &Device, keeper: &str, files: &[&str]) -> i64 {
        let keep = self.reference(keeper);
        let target_endpoint = peer(&self.dir, &target.id).unwrap();
        let request = uuid::Uuid::new_v4().to_string();
        let issuer = Server {
            data_dir: self.dir.clone(),
            cache_dir: None,
        };
        let grant = issuer
            .keep_relay_grant(
                "lisa_keep_relay_issue",
                &json!({"target_device":target.id,"request_id":request,"keep":keep}),
                &Auth::Stdio,
            )
            .unwrap();
        let refs: Vec<_> = files.iter().map(|file| target.reference(file)).collect();
        let response = call_keep_without_signing(&target_endpoint,"lisa_duplicate_plan_create", json!({
            "relay_port":self.port,"relay_secret":grant["secret"],"request_id":request,"keep":keep,"sources":refs,
            "keep_label":"旧PC › fixture › 0.txt",
        })).unwrap();
        response["id"].as_i64().expect("保留計画がある")
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.handle.unblock();
            thread.join().unwrap();
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop();
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn call_keep_without_signing(
    endpoint: &super::Endpoint,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let mut response=ureq::post(format!("http://127.0.0.1:{}/mcp",endpoint.port))
        .header("Authorization",&format!("Bearer {}",endpoint.key))
        .send(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":args}}).to_string().as_bytes())
        .map_err(|e|e.to_string())?;
    let body: Value = serde_json::from_str(&response.body_mut().read_to_string().unwrap()).unwrap();
    body["result"]
        .get("structuredContent")
        .cloned()
        .ok_or_else(|| {
            body["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("拒否")
                .into()
        })
}

fn fixtures() -> (Device, Device) {
    let root = fs::canonicalize(
        std::env::var_os("LISA_VERIFY_KEEP_ROOT").expect("合成ディレクトリを指定"),
    )
    .unwrap()
    .join(uuid::Uuid::new_v4().to_string());
    let a = Device::create(&root, "旧PC");
    let b = Device::create(&root, "新PC");
    a.pair(&b, AB, BA);
    b.pair(&a, BA, AB);
    (a, b)
}

fn execute(target: &Device, id: i64) -> Result<organize::ApplyReport, String> {
    let mut db = target.reopen();
    let mut ctx = Context::with_builtin(target.dir.clone());
    ctx.keep_client = Some(relay_client);
    organize::approve_plan_with_context(&mut db, &ctx, id, false)?;
    organize::execute_plan(&mut db, &ctx, id)
}

#[test]
fn over_http_pending_disconnection_leaves_approval_pending() {
    let (mut a, b) = fixtures();
    let id = a.ask(&b, &["1.txt"]);
    let summary = organize::remote_keep::keep_summary(&b.reopen(), id).unwrap();
    assert_eq!(summary.label.as_deref(), Some("旧PC › fixture › 0.txt"));
    assert_eq!(summary.hash.len(), 12);
    assert!(summary.line().starts_with("残す: 旧PC › fixture › 0.txt"));
    a.stop();
    assert!(execute(&b, id).unwrap_err().contains("Mac"));
    assert_eq!(
        organize::get_plan(&b.reopen(), id)
            .unwrap()
            .unwrap()
            .0
            .state,
        PlanState::Pending
    );
    assert!(b.mount.join("1.txt").is_file());
}

#[test]
fn over_http_concurrent_reverse_requests_cannot_both_isolate() {
    let (a, b) = fixtures();
    let ab = a.ask(&b, &["0.txt"]);
    let ba = b.ask(&a, &["0.txt"]);
    let start = Barrier::new(2);
    let (a_result, b_result) = std::thread::scope(|s| {
        let one = s.spawn(|| {
            start.wait();
            execute(&a, ba)
        });
        start.wait();
        let other = execute(&b, ab);
        (one.join().unwrap(), other)
    });
    let done =
        |report: Result<organize::ApplyReport, String>| report.map_or(0, |report| report.done);
    assert_eq!(done(a_result) + done(b_result), 1);
    assert_ne!(
        a.mount.join("0.txt").exists(),
        b.mount.join("0.txt").exists()
    );
}

static CUT_AFTER_FIRST: Mutex<Option<ListenerHandle>> = Mutex::new(None);
fn cut(
    data: &Path,
    route: &Route,
    keeper: &KeepReference,
    action: RelayAction,
) -> Result<(), String> {
    relay_client(data, route, keeper, action)?;
    if action == RelayAction::Renew
        && let Some(listener) = CUT_AFTER_FIRST.lock().unwrap().take()
    {
        listener.unblock();
    }
    Ok(())
}

#[test]
fn over_http_a_lost_link_stops_the_remaining_op_and_preserves_the_first_mtime() {
    let (a, b) = fixtures();
    *CUT_AFTER_FIRST.lock().unwrap() = Some(a.handle.clone());
    let id = a.ask(&b, &["0.txt", "1.txt", "2.txt"]);
    let mtime = fs::metadata(b.mount.join("0.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let mut db = b.reopen();
    let mut ctx = Context::with_builtin(b.dir.clone());
    ctx.keep_client = Some(cut);
    organize::approve_plan_with_context(&mut db, &ctx, id, false).unwrap();
    let result = organize::execute_plan(&mut db, &ctx, id).unwrap();
    assert_eq!(result.done, 1);
    assert_eq!(result.state, PlanState::Partial);
    assert!(b.mount.join("1.txt").is_file() && b.mount.join("2.txt").is_file());
    let ops = organize::plan_ops(&db, id).unwrap();
    assert_eq!(
        fs::metadata(ops[0].dst.as_ref().unwrap())
            .unwrap()
            .modified()
            .unwrap(),
        mtime
    );
    assert!(ops[2].reason.as_deref().unwrap().contains("再取得"));
    assert!(organize::execute_plan(&mut db, &ctx, id).is_err());
}

fn expire(
    data: &Path,
    route: &Route,
    keeper: &KeepReference,
    action: RelayAction,
) -> Result<(), String> {
    if action == RelayAction::Renew {
        std::thread::sleep(std::time::Duration::from_millis(
            organize::remote_keep::KEEP_LEASE_MS as u64 + 1200,
        ));
    }
    relay_client(data, route, keeper, action)
}

#[test]
fn over_http_expired_reservation_cannot_authorize_isolation() {
    let (a, b) = fixtures();
    let id = a.ask(&b, &["1.txt", "2.txt"]);
    let mut db = b.reopen();
    let mut ctx = Context::with_builtin(b.dir.clone());
    ctx.keep_client = Some(expire);
    organize::approve_plan_with_context(&mut db, &ctx, id, false).unwrap();
    let result = organize::execute_plan(&mut db, &ctx, id).unwrap();
    assert_eq!(result.done, 0);
    let ops = organize::plan_ops(&db, id).unwrap();
    assert!(ops[0].reason.as_deref().unwrap().contains("失効"));
    assert!(ops[1].reason.as_deref().unwrap().contains("再取得"));
    assert!(b.mount.join("1.txt").is_file() && b.mount.join("2.txt").is_file());
}

#[test]
fn over_http_changed_folder_child_refuses_approval() {
    let (a, b) = fixtures();
    let id = a.ask_keep(&b, "folder", &["folder"]);
    fs::write(a.mount.join("folder/x.txt"), b"else").unwrap();
    assert!(execute(&b, id).unwrap_err().contains("変わりました"));
    assert_eq!(
        organize::get_plan(&b.reopen(), id)
            .unwrap()
            .unwrap()
            .0
            .state,
        PlanState::Pending
    );
    assert_eq!(fs::read(b.mount.join("folder/x.txt")).unwrap(), b"same");
}

#[test]
fn over_http_crash_after_first_rename_recovers_without_repeating_it() {
    let (a, b) = fixtures();
    let id = a.ask(&b, &["1.txt", "2.txt"]);
    let binary = std::env::var_os("LISA_VERIFY_SCAN_BIN").expect("合成試験用 CLI を指定");
    let child = std::process::Command::new(binary)
        .args(["plan", "approve", &id.to_string()])
        .env("LISA_DATA_DIR", &b.dir)
        .env("LISA_VERIFY_KEEP_CRASH_AFTER_RENAME", "1")
        .status()
        .unwrap();
    assert_eq!(child.code(), Some(71));
    assert!(!b.mount.join("1.txt").exists() && b.mount.join("2.txt").exists());
    let mut db = b.reopen();
    let mut ctx = Context::with_builtin(b.dir.clone());
    ctx.keep_client = Some(relay_client);
    let recovered = organize::recover_running_plans(&mut db, &ctx).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].done, 2);
    assert!(!b.mount.join("1.txt").exists() && !b.mount.join("2.txt").exists());
    assert!(organize::execute_plan(&mut db, &ctx, id).is_err());
}

#[test]
fn over_http_grant_for_other_physical_copy_cannot_create_another_plan() {
    let (a, b) = fixtures();
    let keep = a.reference("0.txt");
    let other = a.reference("1.txt");
    let request = uuid::Uuid::new_v4().to_string();
    let issuer = Server {
        data_dir: a.dir.clone(),
        cache_dir: None,
    };
    let grant = issuer
        .keep_relay_grant(
            "lisa_keep_relay_issue",
            &json!({"target_device":b.id,"request_id":request,"keep":keep}),
            &Auth::Stdio,
        )
        .unwrap();
    let endpoint = peer(&a.dir, &b.id).unwrap();
    let result = call_keep_without_signing(
        &endpoint,
        "lisa_duplicate_plan_create",
        json!({
            "relay_port":a.port,"relay_secret":grant["secret"],"request_id":request,
            "keep":other,"sources":[b.reference("2.txt")],
        }),
    );
    assert!(result.unwrap_err().contains("保持項目が計画と異なります"));
    assert!(
        organize::list_plans(&b.reopen(), None, 10)
            .unwrap()
            .is_empty()
    );
    assert!(b.mount.join("2.txt").is_file());
}

#[test]
fn over_http_simultaneous_local_approval_executes_only_once() {
    let (a, b) = fixtures();
    let id = a.ask(&b, &["1.txt"]);
    let endpoint = peer(&a.dir, &b.id).unwrap();
    let barrier = Barrier::new(3);
    let binary = std::env::var_os("LISA_VERIFY_SCAN_BIN").expect("合成試験用 CLI を指定");
    let (app, cli, mcp) = std::thread::scope(|scope| {
        let app = scope.spawn(|| {
            barrier.wait();
            execute(&b, id)
        });
        let cli = scope.spawn(|| {
            barrier.wait();
            std::process::Command::new(binary)
                .args(["plan", "approve", &id.to_string()])
                .env("LISA_DATA_DIR", &b.dir)
                .output()
                .unwrap()
                .status
                .success()
        });
        barrier.wait();
        let mcp = call_keep_without_signing(&endpoint, "lisa_plan_approve", json!({"id":id}));
        (app.join().unwrap(), cli.join().unwrap(), mcp)
    });
    assert!(mcp.is_err());
    assert_eq!(app.map_or(0, |result| result.done) + usize::from(cli), 1);
    assert!(!b.mount.join("1.txt").exists());
}

#[test]
fn over_http_one_way_pairing_and_changed_keep_at_execution() {
    let (a, b) = fixtures();
    fs::remove_file(a.dir.join("lan.json")).unwrap();
    fs::remove_file(b.dir.join("peers.json")).unwrap();
    let id = a.ask(&b, &["1.txt"]);
    let mut db = b.reopen();
    let mut ctx = Context::with_builtin(b.dir.clone());
    ctx.keep_client = Some(relay_client);
    organize::approve_plan_with_context(&mut db, &ctx, id, false).unwrap();
    fs::write(a.mount.join("0.txt"), b"else").unwrap();
    let failure = organize::execute_plan(&mut db, &ctx, id).unwrap_err();
    assert!(failure.contains("変わりました"), "{failure}");
    assert_eq!(
        organize::get_plan(&db, id).unwrap().unwrap().0.state,
        PlanState::Pending
    );
    assert_eq!(fs::read(b.mount.join("1.txt")).unwrap(), b"same");
}

#[test]
fn over_http_revoked_grant_cannot_be_used_by_the_target() {
    let (a, b) = fixtures();
    let request = uuid::Uuid::new_v4().to_string();
    let issuer = Server {
        data_dir: a.dir.clone(),
        cache_dir: None,
    };
    let grant = issuer
        .keep_relay_grant(
            "lisa_keep_relay_issue",
            &json!({"target_device":b.id,"request_id":request,"keep":a.reference("0.txt")}),
            &Auth::Stdio,
        )
        .unwrap();
    let counter = |server: &Server| {
        server.handle_keep_relay(
            json!({"counter_query":true,"target_device":b.id,"request_id":request,
                "secret":grant["secret"]})
            .to_string()
            .as_bytes(),
        )
    };
    assert!(counter(&issuer).is_ok());
    issuer
        .keep_relay_grant(
            "lisa_keep_relay_revoke",
            &json!({"target_device":b.id,"request_id":request}),
            &Auth::Stdio,
        )
        .unwrap();
    assert!(counter(&issuer).is_err());
}

/// 大きな保持項目でも、予約から実行までが時間内に終わることの計測（`--ignored` で実行）。
#[test]
#[ignore = "2GB の合成ファイルを作るので手動で実行する"]
fn over_http_large_keep_reserve_to_execution_time() {
    let (a, b) = fixtures();
    for device in [&a, &b] {
        let file = fs::File::create(device.mount.join("big.bin")).unwrap();
        file.set_len(2 << 30).unwrap();
        let mut db = device.reopen();
        db.save_listing(
            "fixture-drive",
            &device.mount,
            &device.mount,
            &listing::read_dir(&device.mount).map_err(|e| e.to_string()),
        )
        .unwrap();
        hash::hash_drive(
            &mut db,
            "fixture-drive",
            &device.mount,
            1,
            &mut |_| {},
            &Pace::FREE,
        )
        .unwrap();
    }
    let id = a.ask_keep(&b, "big.bin", &["big.bin"]);
    let started = std::time::Instant::now();
    let report = execute(&b, id).unwrap();
    eprintln!("2GB 予約〜実行: {:?} / {report:?}", started.elapsed());
    assert_eq!(report.done, 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn over_http_remote_keep_is_not_a_last_copy_warning() {
    let (a, b) = fixtures();
    for device in [&a, &b] {
        fs::write(device.mount.join("only.txt"), b"only here").unwrap();
        let mut db = device.reopen();
        db.save_listing(
            "fixture-drive",
            &device.mount,
            &device.mount,
            &listing::read_dir(&device.mount).map_err(|e| e.to_string()),
        )
        .unwrap();
        hash::hash_drive(
            &mut db,
            "fixture-drive",
            &device.mount,
            1,
            &mut |_| {},
            &Pace::FREE,
        )
        .unwrap();
    }
    let ctx = Context::with_builtin(b.dir.clone());
    let local = organize::create_plan(
        &mut b.reopen(),
        &ctx,
        "test",
        None,
        &[organize::OpInput::Trash {
            path: b.mount.join("only.txt"),
            keep: None,
        }],
        true,
    )
    .unwrap();
    assert_eq!(local.summary.risky, 1);
    let id = a.ask_keep(&b, "only.txt", &["only.txt"]);
    let (plan, ops) = organize::get_plan(&b.reopen(), id).unwrap().unwrap();
    assert_eq!(plan.summary.risky, 0);
    assert!(ops.iter().all(|op| op.risky.is_none()));
}
