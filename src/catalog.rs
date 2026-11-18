//! カタログ（`lisa_core::catalog`）を裏のスレッドで DB の木に当て、当たったフォルダに札を付け、中の
//! `cache`・`install`・`log` を少し暗く描く。右のパネル（`ui/catalog.rs`）に一覧を出す。
//! 表示だけで、ユーザーのファイルは何も動かさない。
//!
//! - 裏のスレッドは 5 秒ごとに、カタログを読むフォルダ（組み込み、設定の `catalog_dirs`、データの
//!   フォルダの `catalogs`）のファイルと、DB の状態（ドライブごとの一番上のハッシュ）を見る。どちらかが
//!   変わったら当て直す（ハッシュの途中は 1 分ごと）。当てた結果は `catalog_match` に書く。
//! - 1 か所ごとの仕分け（`assignment`）も同じスレッドで読み、型の当たりより先にする。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use lisa_core::catalog::{self, CatalogSet, Role, State};
use lisa_core::db::Db;
use lisa_core::query;

use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::BakeFinished;

/// 当て直すかを見る間隔。
const CHECK_EVERY: Duration = Duration::from_secs(5);
/// ハッシュの途中（DB の状態が決まらない）ときに当て直す間隔。
const WHILE_HASHING: Duration = Duration::from_secs(60);
/// 「どれにも当たらない大きなフォルダ」の下限と数。
const UNCLASSIFIED_MIN: u64 = 1 << 30;
const UNCLASSIFIED_LIMIT: usize = 30;

pub struct CatalogPlugin;

impl Plugin for CatalogPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CatalogView>()
            .init_resource::<CatalogMarks>()
            .add_systems(Startup, start_worker)
            .add_systems(Update, (receive, update_marks, order_duplicates).chain());
    }
}

/// 型か仕分けに当たった 1 か所。
#[derive(Clone, Debug)]
pub struct Hit {
    /// ドライブの一番上からの `entry.id`（最後が当たったフォルダ）。
    pub chain: Vec<i64>,
    /// 型の ID。仕分けなら None。
    pub catalog: Option<String>,
    /// 型の名前（仕分けなら「仕分け」）。
    pub name: String,
    pub app: String,
    /// 札の文字。
    pub label: String,
    pub color: Color,
    pub state: State,
    pub path: PathBuf,
    /// フォルダの中の合計（バイト）。
    pub size: u64,
    /// 仕分けの行の番号（仕分けのときだけ）。
    pub assignment: Option<i64>,
}

impl Hit {
    pub fn entry_id(&self) -> i64 {
        *self.chain.last().unwrap_or(&0)
    }
}

/// どの型にも仕分けにも当たらない大きなフォルダ。
#[derive(Clone, Debug)]
pub struct Big {
    pub chain: Vec<i64>,
    pub path: PathBuf,
    pub size: u64,
}

/// 裏のスレッドが当てた結果。
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// 仕分けを先に、そのあと型の当たり（アプリ、型、大きさの順）。同じフォルダは 1 回だけ。
    pub hits: Vec<Hit>,
    /// 当たったフォルダの中の役割（`part`）。
    pub roles: HashMap<i64, Role>,
    /// 読めなかったカタログのファイルと理由。
    pub errors: Vec<String>,
    pub unclassified: Vec<Big>,
    /// 読んだ型の数。
    pub catalogs: usize,
    /// 仕分けに選べるアプリ（型と仕分けに出てくるもの）。
    pub apps: Vec<String>,
}

/// パネルの状態。
#[derive(Resource, Default)]
pub struct CatalogView {
    /// 右のパネルを開いている。
    pub open: bool,
    pub snapshot: Option<Snapshot>,
    /// 選んだ当たり（`snapshot.hits` の番号）。
    pub selected: Option<usize>,
    receiver: Option<Mutex<Receiver<Snapshot>>>,
}

/// 裏のスレッドを起こす口（当て直してほしいとき）。
static WAKE: OnceLock<Mutex<Sender<()>>> = OnceLock::new();

/// すぐ当て直す（仕分けを変えたときなど）。
pub fn refresh_now() {
    if let Some(wake) = WAKE.get()
        && let Ok(wake) = wake.lock()
    {
        let _ = wake.send(());
    }
}

/// フォルダ（この端末の DB の `entry.id`）を `app` 用と仕分ける。書くのは DB の `assignment` だけ。
pub fn assign(entry: i64, app: &str, role: Option<Role>) {
    let app = app.to_string();
    with_db(move |db| catalog::db::assign(db, entry, &app, role, None, "user").map(|_| ()));
}

/// 仕分けを外す（DB の行を消すだけ。ファイルは触らない）。
pub fn unassign(id: i64) {
    with_db(move |db| catalog::db::unassign(db, id).map(|_| ()));
}

/// 裏で DB を開いて書き、済んだら当て直す。
fn with_db(work: impl FnOnce(&mut Db) -> lisa_core::db::Result<()> + Send + 'static) {
    std::thread::spawn(move || {
        let Some(path) = crate::scan::db_path() else {
            return;
        };
        match Db::open(&path).and_then(|mut db| work(&mut db)) {
            Ok(()) => refresh_now(),
            Err(error) => warn!("仕分けを書けませんでした: {error}"),
        }
    });
}

/// 木の上の印（`render/tag.rs` が本物の場面に描く）。
#[derive(Resource, Default, PartialEq)]
pub struct CatalogMarks {
    /// 札（文字と色）。
    pub tags: HashMap<NodeId, (String, Color)>,
    /// 少し暗く描くもの（`cache`・`install`・`log`）。
    pub dim: HashSet<NodeId>,
    /// パネルで選んだ当たり（枠を付ける）とその色。
    pub selected: Option<(NodeId, Color)>,
}

fn start_worker(mut view: ResMut<CatalogView>) {
    let (wake_tx, wake_rx) = channel();
    let (tx, rx) = channel();
    let _ = WAKE.set(Mutex::new(wake_tx));
    view.receiver = Some(Mutex::new(rx));
    let spawned = std::thread::Builder::new()
        .name("lisa-catalog".into())
        .spawn(move || run(tx, wake_rx));
    if let Err(error) = spawned {
        warn!("カタログのスレッドを立てられませんでした: {error}");
    }
}

/// カタログを読むフォルダ（組み込みは別。後のものほど優先）。
pub fn catalog_dirs() -> Vec<PathBuf> {
    let settings = crate::settings::settings_path()
        .map(|p| catalog::CatalogSettings::from_file(&p))
        .unwrap_or_default();
    crate::scan::data_dir()
        .map(|dir| catalog::dirs(&dir, &settings))
        .unwrap_or_default()
}

/// フォルダの中の `*.toml` の名前・大きさ・更新日時（変わったかを見るため）。
fn fingerprint(dirs: &[PathBuf]) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml")
                && let Ok(meta) = entry.metadata()
            {
                out.push((path, meta.len(), meta.modified().ok()));
            }
        }
    }
    out.sort();
    out
}

fn run(tx: Sender<Snapshot>, wake: Receiver<()>) {
    lisa_core::pace::set_thread_background(true);
    let mut last_files = None;
    let mut last_signature = None;
    let mut last_run: Option<Instant> = None;
    let mut forced = true;
    loop {
        let dirs = catalog_dirs();
        let files = fingerprint(&dirs);
        let Some(path) = crate::scan::db_path().filter(|p| p.exists()) else {
            if !sleep(&wake, &mut forced) {
                return;
            }
            continue;
        };
        let mut db = match Db::open(&path) {
            Ok(db) => db,
            Err(error) => {
                warn!("カタログ: DB を開けませんでした: {error}");
                if !sleep(&wake, &mut forced) {
                    return;
                }
                continue;
            }
        };
        let signature = lisa_core::dups::signature(&db).ok().flatten();
        let due = forced
            || last_files.as_ref() != Some(&files)
            || match &signature {
                Some(_) => signature != last_signature,
                None => last_run.is_none_or(|t| t.elapsed() >= WHILE_HASHING),
            };
        if due {
            forced = false;
            last_files = Some(files);
            last_signature = signature;
            last_run = Some(Instant::now());
            let started = Instant::now();
            match refresh(&mut db, &dirs) {
                Ok(snapshot) => {
                    info!(
                        "カタログ: {} か所に当たりました（{:.1} 秒）",
                        snapshot.hits.len(),
                        started.elapsed().as_secs_f32()
                    );
                    if tx.send(snapshot).is_err() {
                        return;
                    }
                }
                Err(error) => warn!("カタログを当てられませんでした: {error}"),
            }
        }
        drop(db);
        if !sleep(&wake, &mut forced) {
            return;
        }
    }
}

/// 次に見るまで待つ。起こされたら `forced` を立てる。UI が閉じたら false。
fn sleep(wake: &Receiver<()>, forced: &mut bool) -> bool {
    match wake.recv_timeout(CHECK_EVERY) {
        Ok(()) => {
            *forced = true;
            true
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
    }
}

fn color(rgb: [u8; 3]) -> Color {
    Color::srgb_u8(rgb[0], rgb[1], rgb[2])
}

/// 当て直して、画面に出すものをまとめる。
fn refresh(db: &mut Db, dirs: &[PathBuf]) -> Result<Snapshot, String> {
    let set = CatalogSet::load_with_builtin(dirs);
    let mut snapshot = Snapshot {
        errors: set
            .errors
            .iter()
            .map(|(path, error)| format!("{}: {error}", path.display()))
            .collect(),
        catalogs: set.catalogs.len(),
        ..default()
    };
    let found = catalog::db::find(db, &set, None, true, &|| true)
        .map_err(|e| e.to_string())?
        .map(|(found, _)| found)
        .unwrap_or_default();
    catalog::db::save(db, &set, &found, None).map_err(|e| e.to_string())?;

    let mut apps: Vec<String> = set.catalogs.iter().map(|l| l.catalog.app.clone()).collect();
    let mut seen = HashSet::new();
    // 仕分けを先に（人や LLM が決めたもの）。
    for (assignment, entry) in catalog::db::resolve_assignments(db).map_err(|e| e.to_string())? {
        apps.push(assignment.app.clone());
        let Some(entry) = entry else {
            continue;
        };
        let Some(located) = query::path_of(db, entry).map_err(|e| e.to_string())? else {
            continue;
        };
        seen.insert(entry);
        if let Some(role) = assignment.role {
            snapshot.roles.insert(entry, role);
        }
        snapshot.hits.push(Hit {
            chain: located.chain,
            catalog: None,
            name: "仕分け".into(),
            app: assignment.app.clone(),
            label: assignment.app.clone(),
            color: color(catalog::app_color(&assignment.app)),
            state: State::Match,
            path: located.path,
            size: query::folder_total(db, entry).map_or(0, |t| t.2),
            assignment: Some(assignment.id),
        });
    }
    let tree = catalog::db::DbTree::new(db);
    let mut hits = Vec::new();
    let mut part_roles = Vec::new();
    for f in &found {
        let Some(loaded) = set.get(&f.catalog_id) else {
            continue;
        };
        // 仕分けたフォルダは、型の当たりより仕分けを先にする（型の方は出さない）。
        if seen.contains(&f.entry_id) {
            continue;
        }
        let Some(located) = query::path_of(db, f.entry_id).map_err(|e| e.to_string())? else {
            continue;
        };
        let cat = &loaded.catalog;
        part_roles.push((located.chain.len(), catalog::parts(cat, &tree, f.entry_id)));
        hits.push(Hit {
            chain: located.chain,
            catalog: Some(cat.id.clone()),
            name: cat.name.clone(),
            app: cat.app.clone(),
            label: cat.label_text(&f.captures),
            color: color(cat.color_rgb()),
            state: f.state,
            path: located.path,
            size: query::folder_total(db, f.entry_id).map_or(0, |t| t.2),
            assignment: None,
        });
    }
    hits.sort_by(|a, b| {
        a.app
            .cmp(&b.app)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| b.size.cmp(&a.size))
    });
    // 内側（深い）に当たった型の役割の方が強いので、深いものから入れる（あとからは上書きしない）。
    part_roles.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    for (_, parts) in part_roles {
        for (entry, role) in parts {
            snapshot.roles.entry(entry).or_insert(role);
        }
    }
    snapshot.hits.extend(hits);
    apps.sort();
    apps.dedup();
    snapshot.apps = apps;
    let matched: HashSet<i64> = snapshot.hits.iter().map(Hit::entry_id).collect();
    snapshot.unclassified =
        catalog::db::unclassified(db, &matched, None, UNCLASSIFIED_MIN, UNCLASSIFIED_LIMIT)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|(entry, size)| {
                let located = query::path_of(db, entry).ok()??;
                Some(Big {
                    chain: located.chain,
                    path: located.path,
                    size,
                })
            })
            .collect();
    Ok(snapshot)
}

/// 裏のスレッドの結果を受け取る。
fn receive(mut view: ResMut<CatalogView>, mut redraw: MessageWriter<RequestRedraw>) {
    let next = {
        let Some(receiver) = &view.receiver else {
            return;
        };
        let Ok(receiver) = receiver.lock() else {
            return;
        };
        receiver.try_iter().last()
    };
    if let Some(snapshot) = next {
        // 選んでいたものは、同じフォルダがあれば選び直す。
        let selected = view
            .selected
            .and_then(|i| view.snapshot.as_ref()?.hits.get(i).map(Hit::entry_id));
        view.selected = selected.and_then(|e| snapshot.hits.iter().position(|h| h.entry_id() == e));
        view.snapshot = Some(snapshot);
        redraw.write(RequestRedraw);
    }
}

/// 当たりと木から、印を付けるノードを決める。変わったら、本物の場面を作り直す。
fn update_marks(
    mut commands: Commands,
    view: Res<CatalogView>,
    tree: Res<FileTree>,
    mut marks: ResMut<CatalogMarks>,
) {
    if !view.is_changed() && !tree.is_changed() {
        return;
    }
    let mut next = CatalogMarks::default();
    if let Some(snapshot) = &view.snapshot {
        let by_entry: HashMap<i64, NodeId> = tree
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.remote.is_none())
            .filter_map(|(id, node)| node.entry.map(|e| (e, id)))
            .collect();
        // 同じフォルダに 2 つ当たったら、仕分けを先に、次に中身から札の文字を取った型（Rust の
        // クレート名など）を、型の名前だけの札（Git リポジトリなど）より先にする。
        let mut best: HashMap<NodeId, (u8, &Hit)> = HashMap::new();
        for hit in &snapshot.hits {
            if let Some(&node) = by_entry.get(&hit.entry_id()) {
                let rank = if hit.assignment.is_some() {
                    0
                } else if hit.label != hit.name {
                    1
                } else {
                    2
                };
                let slot = best.entry(node).or_insert((rank, hit));
                if rank < slot.0 {
                    *slot = (rank, hit);
                }
            }
        }
        for (node, (_, hit)) in best {
            next.tags.insert(node, (hit.label.clone(), hit.color));
        }
        for (entry, role) in &snapshot.roles {
            if matches!(role, Role::Cache | Role::Install | Role::Log)
                && let Some(&node) = by_entry.get(entry)
            {
                next.dim.insert(node);
            }
        }
        if let Some(hit) = view.selected.and_then(|i| snapshot.hits.get(i))
            && let Some(&node) = by_entry.get(&hit.entry_id())
        {
            next.selected = Some((node, hit.color));
        }
    }
    if *marks != next {
        debug!(
            "カタログの印: 札 {}、暗く {}",
            next.tags.len(),
            next.dim.len()
        );
        *marks = next;
        // 印は本物の場面にだけ描くので、場面を作り直す。
        commands.insert_resource(BakeFinished);
    }
}

/// 重複の一覧を、カタログに合わせて並べ直す。`cache`・`install` の中の組は後ろへ、設定の
/// `catalog_hide_duplicates` の型の中の組は出さない（`lisa_core::catalog::order_duplicates`）。
fn order_duplicates(
    view: Res<CatalogView>,
    settings: Res<crate::settings::Settings>,
    mut dups: ResMut<crate::dups::Dups>,
) {
    if !(view.is_changed() || dups.is_changed() || settings.is_changed()) {
        return;
    }
    let Some(snapshot) = &view.snapshot else {
        return;
    };
    let regen: HashSet<i64> = snapshot
        .roles
        .iter()
        .filter(|(_, role)| matches!(role, Role::Cache | Role::Install))
        .map(|(entry, _)| *entry)
        .collect();
    let hidden: HashSet<i64> = snapshot
        .hits
        .iter()
        .filter(|h| {
            h.catalog
                .as_ref()
                .is_some_and(|c| settings.catalog_hide_duplicates.contains(c))
        })
        .map(Hit::entry_id)
        .collect();
    // 並びがもう合っていれば、変えた印を付けない（一覧の作り直しが続かないように）。
    let crate::dups::DupsState::Ready(groups) = &mut dups.bypass_change_detection().state else {
        return;
    };
    if catalog::order_duplicates(groups, &regen, &hidden) {
        dups.selected = None;
        dups.set_changed();
    }
}
