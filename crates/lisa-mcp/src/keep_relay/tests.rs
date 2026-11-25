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

    /// 一番上に `name` を置き、読み直してハッシュする。
    fn add(&self, name: &str, bytes: &[u8]) {
        fs::write(self.mount.join(name), bytes).unwrap();
        let mut db = self.reopen();
        db.save_listing(
            "fixture-drive",
            &self.mount,
            &self.mount,
            &listing::read_dir(&self.mount).map_err(|e| e.to_string()),
        )
        .unwrap();
        hash::hash_drive(
            &mut db,
            "fixture-drive",
            &self.mount,
            1,
            &mut |_| {},
            &Pace::FREE,
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
        self.ask_keep_on(self, target, keeper, files)
    }

    fn ask_keep_on(
        &self,
        keeper_device: &Device,
        target: &Device,
        keeper: &str,
        files: &[&str],
    ) -> i64 {
        let keep = keeper_device.reference(keeper);
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
    ctx.link_client = Some(super::link_relay_client);
    organize::approve_plan_with_context(&mut db, &ctx, id, false)?;
    organize::execute_plan(&mut db, &ctx, id)
}

#[test]
fn over_http_marker_sync_keeps_reservation_until_ack_and_retries_terminal_generation() {
    use lisa_core::merged::{self, links};

    let (a, b) = fixtures();
    let id = a.ask(&b, &["1.txt"]);
    assert_eq!(execute(&b, id).unwrap().done, 1);
    let path = b.mount.join("1.txt.lmf");
    let marker = merged::read(&path).expect("マージ済みリンクを作りました");
    let mut db = b.reopen();
    let keeper = a.reopen();
    let mut ctx = Context::with_builtin(b.dir.clone());
    ctx.link_client = Some(super::link_relay_client);
    let pending = links::sources(&keeper, &marker.target).unwrap();
    assert_eq!(pending[0].state, links::State::Reserved);
    assert_eq!(pending[0].sync_state, "pending");
    assert_eq!(
        links::status(&db, &marker.link_id).unwrap().as_deref(),
        Some("pending")
    );

    organize::flush_links(&mut db, &ctx).unwrap();
    let active = links::sources(&keeper, &marker.target).unwrap();
    assert_eq!(active[0].state, links::State::Active);
    assert_eq!(
        links::status(&db, &marker.link_id).unwrap().as_deref(),
        Some("confirmed")
    );

    fs::remove_file(&path).unwrap();
    organize::flush_links(&mut db, &ctx).unwrap();
    assert!(links::sources(&keeper, &marker.target).unwrap().is_empty());
    assert!(links::permits(&keeper).unwrap()[0].revoked);
    assert!(links::pending(&db).unwrap().is_empty());
}

#[test]
fn over_http_moves_target_and_source_through_a_separate_relay() {
    use lisa_core::merged::{self, links};
    use organize::OpInput;

    let (relay, source) = fixtures();
    let target = Device::create(relay.root.parent().unwrap(), "target");
    target.pair(&relay, "target-to-relay", "relay-to-target");
    fs::write(
        target
            .dir
            .join(organize::remote_keep::relay::DISCOVERY_FILE),
        json!({relay.id.clone(): {"address": format!("127.0.0.1:{}", relay.port),
                                  "seen_at": lisa_core::db::now_ms()}})
        .to_string(),
    )
    .unwrap();
    let peers_path = relay.dir.join("peers.json");
    let mut peers: Vec<Value> = serde_json::from_slice(&fs::read(&peers_path).unwrap()).unwrap();
    peers.push(json!({
        "device_id": target.id, "addr": "127.0.0.1", "port": target.port, "key": "relay-to-target",
    }));
    let online_peers = serde_json::to_vec(&peers).unwrap();
    fs::write(&peers_path, &online_peers).unwrap();

    let merge = relay.ask_keep_on(&target, &source, "0.txt", &["1.txt"]);
    assert_eq!(execute(&source, merge).unwrap().done, 1);
    let mut source_db = source.reopen();
    let mut source_ctx = Context::with_builtin(source.dir.clone());
    source_ctx.link_client = Some(super::link_relay_client);
    organize::flush_links(&mut source_db, &source_ctx).unwrap();
    let source_path = source.mount.join("1.txt.lmf");
    let before = merged::read(&source_path).unwrap();

    let mut target_db = target.reopen();
    let mut target_ctx = Context::with_builtin(target.dir.clone());
    target_ctx.relocation_client = Some(super::relocation_client);
    let destination = target.mount.join("retargeted.txt");
    let move_plan = |db: &mut Db, ctx: &Context, from: PathBuf, to: PathBuf| {
        let created =
            organize::create_plan(db, ctx, "test", None, &[OpInput::Move { from, to }], false)
                .unwrap();
        let id = created.id.unwrap_or_else(|| panic!("{:?}", created.checks));
        organize::approve_plan(db, id, false).unwrap();
        assert_eq!(organize::execute_plan(db, ctx, id).unwrap().done, 1);
        id
    };
    // 中継から目印を持つ端末にだけ届かない状態を再現する。
    peers.retain(|peer| peer["device_id"] != source.id);
    fs::write(&peers_path, serde_json::to_vec(&peers).unwrap()).unwrap();
    let id = move_plan(
        &mut target_db,
        &target_ctx,
        target.mount.join("0.txt"),
        destination.clone(),
    );
    assert_eq!(merged::read(&source_path).unwrap(), before);
    assert_eq!(
        links::status(&target_db, &before.link_id)
            .unwrap()
            .as_deref(),
        Some("pending")
    );
    assert!(
        organize::plan_ops(&target_db, id).unwrap()[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("自動追従")
    );
    let postponed = organize::undo_plan(&mut target_db, &target_ctx, id).unwrap();
    assert_eq!(postponed.undone, 0);
    assert_eq!(postponed.state, PlanState::Done);
    assert!(destination.is_file());
    assert!(merged::resolve_target(&target_db, &before).is_err());
    fs::write(&peers_path, online_peers).unwrap();
    organize::flush_links(&mut target_db, &target_ctx).unwrap();
    let after = merged::read(&source_path).unwrap();
    assert_eq!(after.target.relative_path, ["retargeted.txt"]);
    assert!(merged::resolve_target(&target_db, &after).is_ok());
    assert_eq!(links::permits(&relay.reopen()).unwrap()[0].generation, 3);
    assert_eq!(
        links::status(&target_db, &before.link_id)
            .unwrap()
            .as_deref(),
        Some("confirmed")
    );

    assert_eq!(
        organize::undo_plan(&mut target_db, &target_ctx, id)
            .unwrap()
            .undone,
        1
    );
    assert_eq!(merged::read(&source_path).unwrap(), before);
    assert_eq!(links::permits(&relay.reopen()).unwrap()[0].generation, 4);

    source_db
        .save_listing(
            "fixture-drive",
            &source.mount,
            &source.mount,
            &listing::read_dir(&source.mount).map_err(|error| error.to_string()),
        )
        .unwrap();
    let new_source = source.mount.join("relocated.lmf");
    move_plan(&mut source_db, &source_ctx, source_path, new_source.clone());
    organize::flush_links(&mut source_db, &source_ctx).unwrap();
    let moved_marker = merged::read(&new_source).unwrap();
    assert_eq!(moved_marker.source.relative_path, ["relocated"]);
    assert_eq!(
        links::sources(&target_db, &before.target).unwrap()[0].marker,
        moved_marker
    );
    assert_eq!(links::permits(&relay.reopen()).unwrap()[0].generation, 5);

    let offline_peers: Vec<_> = peers
        .into_iter()
        .filter(|peer| peer["device_id"] != target.id)
        .collect();
    let connected_peers = fs::read(&peers_path).unwrap();
    fs::write(&peers_path, serde_json::to_vec(&offline_peers).unwrap()).unwrap();
    let final_target = target.mount.join("combined.txt");
    let final_source = source.mount.join("combined.lmf");
    move_plan(
        &mut target_db,
        &target_ctx,
        target.mount.join("0.txt"),
        final_target,
    );
    move_plan(
        &mut source_db,
        &source_ctx,
        new_source,
        final_source.clone(),
    );
    assert_eq!(
        links::status(&target_db, &before.link_id)
            .unwrap()
            .as_deref(),
        Some("pending")
    );
    assert_eq!(
        links::status(&source_db, &before.link_id)
            .unwrap()
            .as_deref(),
        Some("pending")
    );
    fs::write(&peers_path, connected_peers).unwrap();
    // 実体側の保留中に目印側から先に再送しても、双方の変更が中継で合流する。
    organize::flush_links(&mut source_db, &source_ctx).unwrap();
    organize::flush_links(&mut target_db, &target_ctx).unwrap();
    organize::flush_links(&mut source_db, &source_ctx).unwrap();
    let combined = merged::read(&final_source).unwrap();
    assert_eq!(combined.source.relative_path, ["combined"]);
    assert_eq!(combined.target.relative_path, ["combined.txt"]);
    assert!(merged::resolve_target(&target_db, &combined).is_ok());
    assert_eq!(
        links::sources(&target_db, &combined.target).unwrap()[0].marker,
        combined
    );
    assert_eq!(links::permits(&relay.reopen()).unwrap()[0].generation, 7);
    assert_eq!(
        links::status(&source_db, &before.link_id)
            .unwrap()
            .as_deref(),
        Some("confirmed")
    );
    assert_eq!(
        links::status(&target_db, &before.link_id)
            .unwrap()
            .as_deref(),
        Some("confirmed")
    );
}

#[test]
fn over_http_restores_a_peer_from_streamed_bytes_when_quarantine_is_absent() {
    use lisa_core::merged::{self, links};
    use organize::{DeleteChoice, DeleteRequest, OpInput};

    let (a, b) = fixtures();
    let merge = a.ask(&b, &["1.txt"]);
    assert_eq!(execute(&b, merge).unwrap().done, 1);
    let mut source = b.reopen();
    let mut source_ctx = Context::with_builtin(b.dir.clone());
    source_ctx.link_client = Some(super::link_relay_client);
    organize::flush_links(&mut source, &source_ctx).unwrap();
    let marker = merged::read(&b.mount.join("1.txt.lmf")).unwrap();
    let mut keeper = a.reopen();
    assert_eq!(
        links::sources(&keeper, &marker.target).unwrap()[0].state,
        links::State::Active
    );
    let quarantined = organize::plan_ops(&source, merge).unwrap()[0]
        .dst
        .clone()
        .unwrap();
    fs::remove_file(quarantined).unwrap();

    let mut ctx = Context::with_builtin(a.dir.clone());
    ctx.relocation_client = Some(super::relocation_client);
    let path = a.mount.join("0.txt");
    let plan = organize::create_plan(
        &mut keeper,
        &ctx,
        "test",
        None,
        &[OpInput::TrashLinked {
            path: path.clone(),
            decision: DeleteRequest {
                choice: DeleteChoice::Restore {
                    link_id: marker.link_id.clone(),
                },
                links: vec![marker.link_id],
            },
        }],
        false,
    )
    .unwrap()
    .id
    .unwrap();
    organize::approve_plan(&mut keeper, plan, false).unwrap();
    let report = organize::execute_plan(&mut keeper, &ctx, plan).unwrap();
    assert_eq!(report.done, 1);
    assert!(!path.exists());
    assert_eq!(fs::read(b.mount.join("1.txt")).unwrap(), b"same");
    assert!(!b.mount.join("1.txt.lmf").exists());
}

#[test]
fn over_http_streams_several_chunks_in_parallel_and_restores_identical_bytes() {
    use lisa_core::merged::{self, links};
    use organize::{DeleteChoice, DeleteRequest, OpInput};

    let (a, b) = fixtures();
    // 4 MiB の小分けが 4 本になり、並行で順不同に届く大きさ。
    let bytes: Vec<u8> = (0..13 * 1024 * 1024 + 7)
        .map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    a.add("big.bin", &bytes);
    b.add("big.bin", &bytes);
    let merge = a.ask_keep(&b, "big.bin", &["big.bin"]);
    assert_eq!(execute(&b, merge).unwrap().done, 1);
    let mut source = b.reopen();
    let mut source_ctx = Context::with_builtin(b.dir.clone());
    source_ctx.link_client = Some(super::link_relay_client);
    organize::flush_links(&mut source, &source_ctx).unwrap();
    let marker = merged::read(&b.mount.join("big.bin.lmf")).unwrap();
    let quarantined = organize::plan_ops(&source, merge).unwrap()[0]
        .dst
        .clone()
        .unwrap();
    fs::remove_file(quarantined).unwrap();

    let mut keeper = a.reopen();
    let mut ctx = Context::with_builtin(a.dir.clone());
    ctx.relocation_client = Some(super::relocation_client);
    let path = a.mount.join("big.bin");
    let plan = organize::create_plan(
        &mut keeper,
        &ctx,
        "test",
        None,
        &[OpInput::TrashLinked {
            path: path.clone(),
            decision: DeleteRequest {
                choice: DeleteChoice::Restore {
                    link_id: marker.link_id.clone(),
                },
                links: vec![marker.link_id.clone()],
            },
        }],
        false,
    )
    .unwrap()
    .id
    .unwrap();
    organize::approve_plan(&mut keeper, plan, false).unwrap();
    assert_eq!(
        organize::execute_plan(&mut keeper, &ctx, plan)
            .unwrap()
            .done,
        1
    );
    assert!(!path.exists());
    assert!(fs::read(b.mount.join("big.bin")).unwrap() == bytes);
    assert!(!b.mount.join("big.bin.lmf").exists());
    assert!(links::sources(&keeper, &marker.target).unwrap().is_empty());
}

#[test]
fn over_http_deleting_an_actual_with_a_peer_source_marks_the_link_missing() {
    use lisa_core::merged::{self, links};
    use organize::{DeleteChoice, DeleteRequest, OpInput};

    let (a, b) = fixtures();
    let merge = a.ask(&b, &["1.txt"]);
    assert_eq!(execute(&b, merge).unwrap().done, 1);
    let mut source = b.reopen();
    let mut source_ctx = Context::with_builtin(b.dir.clone());
    source_ctx.link_client = Some(super::link_relay_client);
    organize::flush_links(&mut source, &source_ctx).unwrap();
    let marker = merged::read(&b.mount.join("1.txt.lmf")).unwrap();

    let mut keeper = a.reopen();
    let ctx = Context::with_builtin(a.dir.clone());
    let path = a.mount.join("0.txt");
    let plan = organize::create_plan(
        &mut keeper,
        &ctx,
        "test",
        None,
        &[OpInput::TrashLinked {
            path: path.clone(),
            decision: DeleteRequest {
                choice: DeleteChoice::DeleteActual,
                links: vec![marker.link_id.clone()],
            },
        }],
        false,
    )
    .unwrap()
    .id
    .unwrap();
    organize::approve_plan(&mut keeper, plan, false).unwrap();
    assert_eq!(
        organize::execute_plan(&mut keeper, &ctx, plan)
            .unwrap()
            .done,
        1
    );
    assert!(!path.exists());
    assert!(b.mount.join("1.txt.lmf").exists(), "別の端末の目印は残す");
    assert_eq!(
        links::status(&keeper, &marker.link_id).unwrap().as_deref(),
        Some("missing")
    );
    organize::undo_plan(&mut keeper, &ctx, plan).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"same");
    assert_eq!(
        links::status(&keeper, &marker.link_id).unwrap().as_deref(),
        Some("confirmed")
    );
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
) -> Result<Option<lisa_core::merged::Location>, String> {
    let location = relay_client(data, route, keeper, action)?;
    if action == RelayAction::Renew
        && let Some(listener) = CUT_AFTER_FIRST.lock().unwrap().take()
    {
        listener.unblock();
    }
    Ok(location)
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
    ctx.link_client = Some(super::link_relay_client);
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
) -> Result<Option<lisa_core::merged::Location>, String> {
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
    ctx.link_client = Some(super::link_relay_client);
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
    ctx.link_client = Some(super::link_relay_client);
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
    ctx.link_client = Some(super::link_relay_client);
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
