//! 読む係（裏のスレッド 1 本）。ドライブを深さ優先で読み、読み終えたらハッシュし、そのあとは頼みを
//! 待つ。`docs/scan.md` の「裏で読む分」の決まりに従う:
//!
//! - 見ているところ（開いたフォルダ）の頼み（`Request::Visible`）が来たら、裏の分はすぐやめて
//!   （ハッシュは 1 MiB ごとに合図を見る）、先にそれを 1 段読み直す。DB と違えば DB を直し、変わった
//!   ものとそのフォルダ・祖先のハッシュを消して（古い）、ハッシュし直す。これは決まりにかからない。
//! - 裏の分は I/O を後回しにし（`pace::set_thread_background`）、速さの上限（設定）を守る。
//! - 一時停止、電池で動いているとき、CPU が混んでいるとき、ほかのアプリがディスクをたくさん読み書き
//!   しているとき（設定のしきい値）は休む。
//! - 監視（`Request::Hint`、FSEvents など）で変わったと知らされたフォルダは、裏の分として（上の
//!   決まりに従って）読み直す。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::log::warn;
use bevy::winit::EventLoopProxy;
use bevy::winit::WinitUserEvent;
use lisa_core::db::{
    self, Db, ErrorCode, HASH_SCHEDULED, HASH_VISIBLE, HashQueueSeed, ListingSave,
};
use lisa_core::hash::{self, Pace};
use lisa_core::listing;

mod manual;
use lisa_core::volumes::{mount_identity, mount_matches};

use super::{Message, Progress, RefreshRequest, RefreshTarget, Root, SHOWN_NEXT, readable, report};

/// 初回走査で、ロックの外で先読みするフォルダ数と項目数の目安。
const INITIAL_FOLDERS: usize = 64;
const INITIAL_ENTRIES: usize = 512;
const INITIAL_READ_BUDGET: Duration = Duration::from_millis(12);
const INITIAL_WRITE_BUDGET: Duration = Duration::from_millis(20);
/// 休む・止めるかを見直す間隔。
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// 前回のイベント番号を覚えておく `meta` の鍵。
#[cfg(target_os = "macos")]
const EVENT_ID_KEY: &str = "fsevents_event_id";
/// 休んでいるあいだ、合図を見に行く間隔。
const NAP: Duration = Duration::from_millis(250);

/// UI から読む係への頼み。
pub enum Request {
    /// 共有の手動更新キューを見に行く。
    ManualWake,
    /// このフォルダを今見ている（1 段読み直し、ハッシュの無いものをハッシュする）。
    Visible(PathBuf),
    /// 整理で DB も更新済みのフォルダを、一覧とプレビューに反映する。
    Refresh(PathBuf),
    /// このフォルダの中が変わったらしい（監視からの知らせ。裏の分として読み直す）。
    Hint(PathBuf),
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: 1 段読み直し、DB と同じでも `Changed -> Listed -> Committed` を送る。
    DebugForceChanged(PathBuf),
}

/// 休んでいる理由（`Control::resting`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rest {
    No = 0,
    Paused = 1,
    Battery = 2,
    Cpu = 3,
    Disk = 4,
}

const ACTIVE: u64 = 1;
const BOOSTED: u64 = 2;
const PRIORITY_FAILED: u64 = 4;
const MODE_FLAGS: u64 = ACTIVE | BOOSTED | PRIORITY_FAILED;
const MODE_REVISION: u64 = MODE_FLAGS + 1;

#[derive(Default)]
struct ManualQueue {
    pending: VecDeque<RefreshRequest>,
    active: Option<RefreshRequest>,
    processed: HashSet<PathBuf>,
}

/// UI と読む係で分け合う、裏で読むときの決まり。
pub struct Control {
    /// UI が走査結果の受け取りを終えた。
    pub(super) closed: AtomicBool,
    /// 一時停止（メニューのボタン）。
    pub paused: AtomicBool,
    /// まだ済んでいない見ているところの頼みの数。0 でなければ、裏の分はやめる。
    pub urgent: AtomicUsize,
    manual_pending: AtomicBool,
    created_pending: AtomicBool,
    manual_sequence: AtomicU64,
    manual: Mutex<ManualQueue>,
    /// 裏の分の速さの上限（バイト毎秒、0 なら上限なし）。
    pub limit: AtomicU64,
    /// 電池で動いているときは休む。
    pub rest_on_battery: AtomicBool,
    /// CPU の混み具合（%）がこれを超えたら休む（100 なら休まない）。
    pub cpu_limit: AtomicU32,
    /// ほかのアプリのディスクの読み書き（バイト毎秒）がこれを超えたら休む（0 なら休まない）。
    pub disk_limit: AtomicU64,
    /// 今休んでいる理由（`Rest`）。
    pub resting: AtomicU8,
    /// 作業中か、希望する優先度、優先度変更の失敗と、その変更の世代。
    mode: AtomicU64,
    #[cfg(all(debug_assertions, feature = "verify-fs"))]
    fail_boost_priority: bool,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            closed: AtomicBool::new(false),
            paused: AtomicBool::new(
                cfg!(all(windows, feature = "verify-fs"))
                    && std::env::var_os("LISA_VERIFY_SCAN_ROOT").is_some(),
            ),
            urgent: AtomicUsize::new(0),
            manual_pending: AtomicBool::new(false),
            created_pending: AtomicBool::new(false),
            manual_sequence: AtomicU64::new(0),
            manual: Mutex::new(ManualQueue::default()),
            limit: AtomicU64::new(0),
            rest_on_battery: AtomicBool::new(true),
            cpu_limit: AtomicU32::new(100),
            disk_limit: AtomicU64::new(0),
            resting: AtomicU8::new(Rest::No as u8),
            mode: AtomicU64::new(0),
            #[cfg(all(debug_assertions, feature = "verify-fs"))]
            fail_boost_priority: crate::remote::debug_remote_enabled()
                && std::env::var_os("LISA_DEBUG_BOOST_PRIORITY_FAILURE").is_some(),
        }
    }
}

impl Control {
    pub(super) fn queue_refresh(
        &self,
        target: RefreshTarget,
        created: Option<(PathBuf, Sender<Option<String>>)>,
    ) -> Option<u64> {
        let mut queue = self.manual.lock().ok()?;
        if created.is_none()
            && let Some(request) = queue.active.as_ref()
            && request.created.is_none()
            && request.target.covers(&target)
            && !queue.processed.iter().any(|path| target.contains(path))
        {
            return Some(request.generation);
        }
        if created.is_none()
            && let Some(request) = queue
                .pending
                .iter()
                .find(|request| request.created.is_none() && request.target.covers(&target))
        {
            return Some(request.generation);
        }
        if queue.pending.len() >= 256 {
            return None;
        }
        queue.pending.retain(|request| {
            created.is_some() || request.created.is_some() || !target.covers(&request.target)
        });
        let generation = self.manual_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let request = RefreshRequest {
            target,
            generation,
            created,
        };
        if request.created.is_some() {
            let index = queue
                .pending
                .iter()
                .position(|pending| pending.created.is_none())
                .unwrap_or(queue.pending.len());
            queue.pending.insert(index, request);
            self.created_pending.store(true, Ordering::Relaxed);
        } else {
            queue.pending.push_back(request);
        }
        self.manual_pending.store(true, Ordering::Relaxed);
        Some(generation)
    }

    pub fn boost_status(&self) -> (bool, bool, bool) {
        let mode = self.mode.load(Ordering::Relaxed);
        (
            mode & ACTIVE != 0,
            mode & BOOSTED != 0,
            mode & PRIORITY_FAILED != 0,
        )
    }

    pub fn set_boost(&self, on: bool) {
        let desired = if on { BOOSTED } else { 0 };
        let _ = self
            .mode
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |mode| {
                (mode & ACTIVE != 0 && mode & BOOSTED != desired)
                    .then(|| (mode & !MODE_FLAGS).wrapping_add(MODE_REVISION) | ACTIVE | desired)
            });
    }

    fn set_active(&self, active: bool) {
        let desired = if active { ACTIVE } else { 0 };
        let _ = self
            .mode
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |mode| {
                (mode & ACTIVE != desired)
                    .then(|| (mode & !MODE_FLAGS).wrapping_add(MODE_REVISION) | desired)
            });
    }

    fn apply_priority(&self) {
        let mode = self.mode.load(Ordering::Relaxed);
        let background = mode & BOOSTED == 0;
        #[cfg(all(debug_assertions, feature = "verify-fs"))]
        let simulated_failure = !background && self.fail_boost_priority;
        #[cfg(not(all(debug_assertions, feature = "verify-fs")))]
        let simulated_failure = false;
        let result = if simulated_failure {
            Err(-1)
        } else {
            lisa_core::pace::try_set_thread_background(background, mode & !MODE_FLAGS)
        };
        #[cfg(all(windows, feature = "verify-fs"))]
        if result == Ok(true) && std::env::var_os("LISA_VERIFY_SCAN_ROOT").is_some() {
            bevy::log::info!(
                "検証: SetThreadPriority 成功（裏の分: {background}、スレッド: {:?}）",
                std::thread::current().id()
            );
        }
        if let Err(error) = result
            && self.mark_priority_failure(mode & !MODE_FLAGS)
        {
            warn!("OS優先度の変更に失敗（裏の分: {background}、エラー: {error}）");
        }
    }

    fn mark_priority_failure(&self, revision: u64) -> bool {
        // 世代の照合と失敗フラグの更新を、同じ CAS で行う。
        self.mode
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |mode| {
                (mode & !MODE_FLAGS == revision && mode & PRIORITY_FAILED == 0)
                    .then_some(mode | PRIORITY_FAILED)
            })
            .is_ok()
    }

    pub fn rest(&self) -> Rest {
        match self.resting.load(Ordering::Relaxed) {
            1 => Rest::Paused,
            2 => Rest::Battery,
            3 => Rest::Cpu,
            4 => Rest::Disk,
            _ => Rest::No,
        }
    }
}

#[derive(Default)]
struct RestChecks {
    sampled: Option<Instant>,
    cpu_revision: u64,
    battery: bool,
    cpu: bool,
    disk: bool,
}

impl RestChecks {
    fn reason(&self, boosted: bool) -> Rest {
        if self.battery {
            Rest::Battery
        } else if !boosted && self.cpu {
            Rest::Cpu
        } else if self.disk {
            Rest::Disk
        } else {
            Rest::No
        }
    }
}

/// 裏の分の読み方（速さの上限と、休むかどうか）。
struct Background {
    control: Arc<Control>,
    /// 速さの上限を守るための、この秒の始まりと、その秒に読んだ量。
    window: Mutex<(Instant, u64)>,
    /// 最後に電池と CPU とディスクを見た時刻と、そのときの答え。
    checked: Mutex<RestChecks>,
    /// 前に見たディスクの読み書きの累計と、それから自分で読んだ量（ほかのアプリの分を出すため）。
    disk: Mutex<Option<u64>>,
    ours: AtomicU64,
}

impl Background {
    fn new(control: Arc<Control>) -> Self {
        Self {
            control,
            window: Mutex::new((Instant::now(), 0)),
            checked: Mutex::new(RestChecks::default()),
            disk: Mutex::new(None),
            ours: AtomicU64::new(0),
        }
    }

    /// 今休むべきか（理由）。電池と CPU は `CHECK_EVERY` ごとに見直す。
    fn rest(&self) -> Rest {
        let c = &self.control;
        let rest = if c.paused.load(Ordering::Relaxed) {
            Rest::Paused
        } else {
            let mut checked = self.checked.lock().expect("休むかどうか");
            let mode = c.mode.load(Ordering::Relaxed);
            let boosted = mode & BOOSTED != 0;
            let revision = mode & !MODE_FLAGS;
            let sample = checked.sampled.is_none_or(|t| t.elapsed() >= CHECK_EVERY);
            if !boosted && (sample || checked.cpu_revision != revision) {
                let limit = c.cpu_limit.load(Ordering::Relaxed);
                checked.cpu = limit < 100
                    && lisa_core::pace::cpu_load_percent().is_some_and(|p| p > limit as f32);
                checked.cpu_revision = revision;
            }
            if sample {
                let seconds = checked.sampled.map(|t| t.elapsed().as_secs_f64());
                checked.battery =
                    c.rest_on_battery.load(Ordering::Relaxed) && lisa_core::pace::on_battery();
                // ほかのアプリのディスクの読み書き = 全体の増えた分 − 自分で読んだ分。
                let disk_limit = c.disk_limit.load(Ordering::Relaxed);
                let total = lisa_core::pace::disk_bytes();
                let ours = self.ours.swap(0, Ordering::Relaxed);
                let mut last = self.disk.lock().expect("ディスク");
                checked.disk = match (disk_limit, *last, total, seconds) {
                    (1.., Some(before), Some(now), Some(seconds)) if seconds > 0.0 => {
                        let others = now.saturating_sub(before).saturating_sub(ours);
                        others as f64 / seconds > disk_limit as f64
                    }
                    _ => false,
                };
                *last = total;
                checked.sampled = Some(Instant::now());
            }
            checked.reason(boosted)
        };
        c.resting.store(rest as u8, Ordering::Relaxed);
        rest
    }

    /// 見ているところの頼みが待っている。
    fn urgent(&self) -> bool {
        self.control.urgent.load(Ordering::Relaxed) > 0
            || self.control.manual_pending.load(Ordering::Relaxed)
    }

    /// 裏の分を続けてよいか。`bytes` だけ読んだので、速さの上限を超えていれば待つ。
    fn step(&self, bytes: u64, cancelled: impl Fn() -> bool) -> bool {
        self.ours.fetch_add(bytes, Ordering::Relaxed);
        loop {
            if self.control.closed.load(Ordering::Relaxed) || self.urgent() || cancelled() {
                return false;
            }
            match self.rest() {
                Rest::No => break,
                // ファイルを読んでいる間は DB の書き込みの鍵がない。CPU 待機だけなら
                // 読んだ分を捨てずに、次のブーストや負荷の低下まで待てる。
                Rest::Cpu if bytes > 0 => {
                    std::thread::sleep(NAP);
                    self.control.apply_priority();
                }
                _ => return false,
            }
        }
        let limit = self.control.limit.load(Ordering::Relaxed);
        if limit == 0 || bytes == 0 {
            return true;
        }
        let wait = {
            let mut window = self.window.lock().expect("速さ");
            if window.0.elapsed() >= Duration::from_secs(1) {
                *window = (Instant::now(), 0);
            }
            window.1 += bytes;
            let allowed = window.0.elapsed().as_secs_f64();
            let needed = window.1 as f64 / limit as f64;
            (needed - allowed).max(0.0)
        };
        if wait > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(wait.min(1.0)));
        }
        !self.control.closed.load(Ordering::Relaxed) && !self.urgent() && !cancelled()
    }
}

/// 読む係が持つもの。
pub(super) struct Worker {
    pub db: Option<Db>,
    pub roots: Vec<Root>,
    pub sender: Sender<Message>,
    pub requests: Receiver<Request>,
    pub proxy: Option<EventLoopProxy<WinitUserEvent>>,
    pub progress: Arc<Mutex<Progress>>,
    pub control: Arc<Control>,
    /// 一覧の変更や期限到達で、永続キューに実行可能な仕事がある。
    pub dirty: bool,
    pub hash_retry_needed: bool,
    pub hash_stats_at: Option<Instant>,
    pub pending_visible: Option<PathBuf>,
    /// 監視から知らされた、読み直すフォルダ（同じものは 1 つにまとめる）。
    pub hints: std::collections::BTreeSet<PathBuf>,
    /// DB の競合で強制読み直しを保留したフォルダ。
    pub refresh_hints: std::collections::BTreeSet<PathBuf>,
    /// 隔離フォルダなど、読む係が飛ばすフォルダの絶対パス。
    pub skip_dirs: Vec<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RereadMode {
    Detect,
    Debug,
    Organize,
}

enum StoreOutcome {
    Published {
        alive: bool,
        changed: bool,
        first: bool,
    },
    Busy,
}

fn finish_initial_store(
    outcome: StoreOutcome,
    stack: &mut Vec<(usize, PathBuf)>,
    index: usize,
    path: PathBuf,
    failed: bool,
) -> (bool, u64, u64) {
    match outcome {
        StoreOutcome::Busy => {
            stack.push((index, path));
            (true, 0, 0)
        }
        StoreOutcome::Published { alive, .. } => (alive, 1, u64::from(failed)),
    }
}

fn push_initial_children(
    stack: &mut Vec<(usize, PathBuf)>,
    index: usize,
    path: &Path,
    result: &Result<listing::Listing, String>,
) {
    if let Ok(listing) = result {
        stack.extend(
            listing
                .entries
                .iter()
                .rev()
                .filter(|entry| readable(entry))
                .map(|entry| (index, path.join(&entry.name))),
        );
    }
}

impl Worker {
    pub fn run(mut self) {
        let background = Background::new(self.control.clone());
        self.run_jobs(&background);
        self.control.set_active(false);
    }

    fn run_jobs(&mut self, background: &Background) {
        // 前回終わってから変わったところ（Mac の FSEvents の履歴）を、読み直しの知らせにする。
        self.changes_since_last_run();
        let mut stack = self.initial_stack();
        self.control
            .set_active(!stack.is_empty() || self.db.is_some());
        if stack.is_empty() {
            report(&self.progress, |p| p.finished = Some(Instant::now()));
        }
        // 1. 親から順に読む。
        let mut batch_limit = INITIAL_FOLDERS;
        while !stack.is_empty() {
            if !self.wait_turn(background) {
                return;
            }
            let (alive, done, failed) = self.initial_batch(&mut stack, &mut batch_limit);
            report(&self.progress, |p| {
                p.done += done;
                p.failed += failed;
                p.queued = stack.len();
                p.next = stack
                    .iter()
                    .rev()
                    .take(SHOWN_NEXT)
                    .map(|(_, p)| p.clone())
                    .collect();
                if stack.is_empty() {
                    p.current = None;
                    p.finished = Some(Instant::now());
                }
            });
            if !alive {
                return;
            }
        }
        lisa_core::pace::set_thread_background(false);
        if let Some(db) = self.db.as_mut() {
            let _ = db.commit();
        }
        if !self.send(Message::Committed) {
            return;
        }
        // 2. 旧 DB を補完し、期限が来た仕事を優先順に処理する。
        if self.db.is_some() {
            report(&self.progress, |p| p.hash.started = Some(Instant::now()));
            if !self.seed_hash_queue() || !self.hash_all(background) {
                return;
            }
            self.control.set_active(false);
            self.snapshot_all(background, true);
            report(&self.progress, |p| {
                p.hash.folder = None;
                p.hash.finished = Some(Instant::now());
            });
            self.wake();
        }
        self.control.set_active(false);
        // 3. あとは頼みを待つ。監視の知らせは裏の分として読み直し、ハッシュを消したら、裏で祖先を
        //    作り直す。
        loop {
            if !self.hints.is_empty() || (self.dirty && self.db.is_some()) {
                self.control.set_active(true);
                report(&self.progress, |p| {
                    *p = Progress {
                        started: Some(Instant::now()),
                        ..Default::default()
                    };
                });
                self.wake();
            }
            while !self.hints.is_empty() {
                if !self.wait_turn(background) {
                    return;
                }
                let Some(path) = self.hints.pop_first() else {
                    break;
                };
                report(&self.progress, |p| {
                    p.current = Some(path.clone());
                    p.queued = self.hints.len();
                });
                let mode = if self.refresh_hints.remove(&path) {
                    RereadMode::Organize
                } else {
                    RereadMode::Detect
                };
                if !self.reread_mode(&path, true, mode) {
                    return;
                }
                self.dirty |= self.hash_ready(HASH_SCHEDULED).unwrap_or(true);
                report(&self.progress, |p| p.done += 1);
            }
            if self.dirty && self.db.is_some() {
                self.dirty = false;
                let seed_again = std::mem::take(&mut self.hash_retry_needed);
                report(&self.progress, |p| {
                    p.current = None;
                    p.finished = Some(Instant::now());
                    p.hash.started = p.finished;
                });
                if (seed_again && !self.seed_hash_queue()) || !self.hash_all(background) {
                    return;
                }
                self.control.set_active(false);
                self.snapshot_all(background, false);
                report(&self.progress, |p| {
                    p.hash.folder = None;
                    p.hash.finished = Some(Instant::now());
                });
                self.wake();
            }
            if self.control.boost_status().0 {
                self.control.set_active(false);
                self.control.apply_priority();
                report(&self.progress, |p| {
                    p.current = None;
                    p.finished = Some(Instant::now());
                });
                self.wake();
            }
            self.update_hash_queue_status(true);
            let wait = self.hash_idle_wait().unwrap_or(NAP);
            match self.requests.recv_timeout(wait) {
                Ok(request) => {
                    if !self.handle(request) {
                        return;
                    }
                }
                // 暇なときに、ここまでの変化は見たという印を付け直す（次の起動の「前回」）。
                Err(RecvTimeoutError::Timeout) => {
                    self.remember_event_id();
                    if self.refresh_hash_mounts() && !self.seed_hash_queue() {
                        return;
                    }
                    self.dirty |= self.hash_retry_needed
                        || self.pending_visible.is_some()
                        || self.hash_ready(HASH_SCHEDULED).unwrap_or(true);
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// 前回覚えたイベント番号から今までに変わったフォルダを、読み直しの知らせにする（Mac のみ）。
    fn changes_since_last_run(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let Some(db) = self.db.as_ref() else {
                return;
            };
            let since = db
                .meta(EVENT_ID_KEY)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok());
            let now = lisa_core::fsevents::current_event_id();
            if let Some(since) = since.filter(|&s| s < now) {
                let roots: Vec<&Path> = self
                    .roots
                    .iter()
                    .filter(|r| r.drive_id.is_some())
                    .map(|r| r.mount.as_path())
                    .collect();
                match lisa_core::fsevents::since(&roots, since, Duration::from_secs(30)) {
                    Some(history) => {
                        if history.dropped {
                            warn!("前回からの変化の一部が FSEvents で捨てられていました");
                        }
                        let before = self.hints.len();
                        for folder in history.folders {
                            if let Some(path) = self.locate(&folder)
                                && !self.is_skipped(&path)
                            {
                                self.hints.insert(path);
                            }
                        }
                        bevy::log::info!(
                            "前回から変わったフォルダ: {} 個",
                            self.hints.len() - before
                        );
                    }
                    None => warn!("前回からの変化（FSEvents の履歴）をもらえませんでした"),
                }
            }
            self.remember_event_id();
        }
    }

    /// 今のイベント番号を覚える（ここまでの変化は、もう知らせとして受け取った）。
    fn remember_event_id(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(db) = self.db.as_mut() {
            let now = lisa_core::fsevents::current_event_id();
            if let Err(error) = db.set_meta(EVENT_ID_KEY, &now.to_string()) {
                warn!("イベント番号を覚えられませんでした: {error}");
            }
        }
    }

    /// ハッシュがそろったドライブのスナップショットを撮る（一番上のハッシュが前と違えば）。その前に、
    /// 前の版で作ったフォルダの中身（`tree` の表）を、裏の分として埋める。
    fn snapshot_all(&mut self, background: &Background, fill_old_trees: bool) {
        lisa_core::pace::set_thread_background(true);
        let Some(db) = self.db.as_mut() else {
            return;
        };
        if fill_old_trees {
            let step = || background.step(0, || false);
            match db.fill_trees(&step) {
                Ok(n) if n > 0 => bevy::log::info!("フォルダの中身を {n} 個残しました"),
                Ok(_) => {}
                Err(error) => warn!("フォルダの中身を残せませんでした: {error}"),
            }
        }
        for root in &self.roots {
            let Some(drive_id) = &root.drive_id else {
                continue;
            };
            match db.take_snapshot(drive_id) {
                Ok(Some(id)) => bevy::log::info!(
                    "{} のスナップショットを撮りました（{id}）",
                    root.mount.display()
                ),
                Ok(None) => {}
                Err(error) => warn!("スナップショットを撮れませんでした: {error}"),
            }
        }
    }

    fn seed_hash_queue(&mut self) -> bool {
        let mut cursors: HashMap<String, HashQueueSeed> = HashMap::new();
        for index in 0..self.roots.len() {
            let Some(id) = self.roots[index].drive_id.clone() else {
                continue;
            };
            while Self::mount_connected(&self.roots[index]) {
                if !self.service_requests() {
                    return false;
                }
                let Some(db) = &mut self.db else { return true };
                match db.seed_hash_queue(&id, cursors.entry(id.clone()).or_default()) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => {
                        warn!("ハッシュキューを補完できませんでした: {error}");
                        self.hash_retry_needed = true;
                        break;
                    }
                }
            }
        }
        true
    }

    fn mount_connected(root: &Root) -> bool {
        mount_matches(&root.mount, root.mount_identity)
    }

    /// 抜き差しでデバイス番号が変わっても、同じボリュームだけを同じルートで再開する。
    fn refresh_hash_mounts(&mut self) -> bool {
        if !self
            .roots
            .iter()
            .any(|root| root.drive_id.is_some() && !Self::mount_connected(root))
        {
            return false;
        }
        let volumes = super::scan_volumes();
        let mut reconnected = false;
        for root in &mut self.roots {
            if root.drive_id.is_some()
                && !Self::mount_connected(root)
                && volumes
                    .iter()
                    .any(|volume| volume.id == root.drive_id && volume.mount_path == root.mount)
            {
                root.mount_identity = mount_identity(&root.mount);
                reconnected |= root.mount_identity.is_some();
            }
        }
        reconnected
    }

    fn hash_idle_wait(&self) -> db::Result<Duration> {
        if !self.hints.is_empty() || self.hash_retry_needed || self.pending_visible.is_some() {
            return Ok(NAP);
        }
        let Some(db) = &self.db else {
            return Ok(Duration::from_secs(60));
        };
        let drives: Vec<_> = self
            .roots
            .iter()
            .filter(|root| Self::mount_connected(root))
            .filter_map(|root| root.drive_id.as_deref())
            .collect();
        let now = db::now_ms();
        if !db
            .pick_hash_queue(&drives, now, 1, HASH_SCHEDULED)?
            .is_empty()
        {
            return Ok(NAP);
        }
        Ok(db
            .next_hash_due_at(&drives, now)?
            .map_or(Duration::from_secs(60), |due| {
                Duration::from_millis(due.saturating_sub(now).clamp(1_000, 60_000) as u64)
            }))
    }

    fn hash_ready(&self, max_priority: i64) -> db::Result<bool> {
        let Some(db) = &self.db else { return Ok(false) };
        let drives: Vec<_> = self
            .roots
            .iter()
            .filter(|root| Self::mount_connected(root))
            .filter_map(|root| root.drive_id.as_deref())
            .collect();
        Ok(!db
            .pick_hash_queue(&drives, db::now_ms(), 1, max_priority)?
            .is_empty())
    }

    fn update_hash_queue_status(&mut self, force: bool) {
        if !crate::remote::debug_remote_enabled()
            || (!force && self.hash_stats_at.is_some_and(|at| at.elapsed() < NAP))
        {
            return;
        }
        let Some(db) = &self.db else { return };
        let drives: Vec<_> = self
            .roots
            .iter()
            .filter(|root| Self::mount_connected(root))
            .filter_map(|root| root.drive_id.as_deref())
            .collect();
        let now = db::now_ms();
        if let (Ok(counts), Ok(next_due_at)) = (
            db.hash_queue_counts(&drives, now),
            db.next_hash_due_at(&drives, now),
        ) {
            let (active, boosted, _) = self.control.boost_status();
            self.hash_stats_at = Some(Instant::now());
            report(&self.progress, |p| {
                p.hash_queue = Some(lisa_core::remote::HashQueueState {
                    ready: counts.total - counts.waiting,
                    waiting: counts.waiting,
                    next_due_at,
                    active,
                    boosted,
                });
            });
            self.wake();
        }
    }

    /// 表示中の仕事を先に済ませ、残りは裏の分のペースで処理する。
    fn hash_all(&mut self, background: &Background) -> bool {
        loop {
            if !self.service_requests() || !self.hash_visible() {
                return false;
            }
            match self.hash_ready(HASH_SCHEDULED) {
                Ok(false) => return true,
                Ok(true) => {}
                Err(error) => {
                    warn!("ハッシュキューを読めませんでした: {error}");
                    self.hash_retry_needed = true;
                    return true;
                }
            }
            if !self.wait_turn(background) {
                return false;
            }
            let interrupted = AtomicBool::new(false);
            let step = |bytes| {
                let go = background.step(bytes, || interrupted.load(Ordering::Relaxed));
                if !go {
                    interrupted.store(true, Ordering::Relaxed);
                }
                go
            };
            let priority = || background.control.apply_priority();
            let pace = Pace {
                background: true,
                priority: Some(&priority),
                step: &step,
            };
            match self.hash_batch(HASH_SCHEDULED, &pace) {
                Some(batch) if batch.picked > 0 => {
                    if !self.publish_hash_batch(batch) {
                        return false;
                    }
                }
                _ => return true,
            }
        }
    }

    /// 親が確定したフォルダを、同じドライブから少しだけ先読みする。
    fn initial_batch(
        &mut self,
        stack: &mut Vec<(usize, PathBuf)>,
        batch_limit: &mut usize,
    ) -> (bool, u64, u64) {
        let Some((index, path)) = stack.pop() else {
            return (true, 0, 0);
        };
        if self.is_skipped(&path) {
            return (true, 0, 0);
        }
        report(&self.progress, |p| {
            p.current = Some(path.clone());
            p.queued = stack.len();
        });
        let root = &self.roots[index];
        let version = self
            .db
            .as_ref()
            .zip(root.drive_id.as_ref())
            .and_then(|(db, _)| db.listing_version().ok());
        let Some(version) = version else {
            return self.store_initial_folder(stack, index, path);
        };
        let started = Instant::now();
        let result = Self::read_dir_filtered(&self.skip_dirs, &path);
        let mut entries = result.as_ref().map_or(0, |listing| listing.entries.len());
        let mut batch = vec![(path, result)];
        while batch.len() < *batch_limit
            && entries < INITIAL_ENTRIES
            && started.elapsed() < INITIAL_READ_BUDGET
            && stack.last().is_some_and(|(next, _)| *next == index)
            && let Some((_, path)) = stack.pop()
        {
            if !self.is_skipped(&path) {
                let result = Self::read_dir_filtered(&self.skip_dirs, &path);
                entries += result.as_ref().map_or(0, |listing| listing.entries.len());
                batch.push((path, result));
            }
        }
        let saved = match self.db.as_mut().zip(root.drive_id.as_ref()) {
            Some((db, drive_id)) => db.save_initial_listings(
                drive_id,
                &root.mount,
                version,
                batch.iter().map(|(path, result)| (path.as_path(), result)),
                INITIAL_WRITE_BUDGET,
            ),
            None => Ok(Vec::new()),
        };
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) => {
                warn!("初回の一覧をまとめて確定できませんでした: {error}");
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) {
                    stack.extend(batch.into_iter().rev().map(|(path, _)| (index, path)));
                    return (true, 0, 0);
                }
                Vec::new()
            }
        };
        if saved.is_empty() {
            *batch_limit = (batch.len() / 2).max(1);
            // 世代が変わった一覧は捨て、先頭を従来の再試行経路で読み直す。
            let mut queued = batch.into_iter().map(|(path, _)| path);
            let path = queued.next().expect("バッチの先頭");
            stack.extend(queued.rev().map(|path| (index, path)));
            return self.store_initial_folder(stack, index, path);
        }
        *batch_limit = (*batch_limit + 1).min(INITIAL_FOLDERS);
        let done = saved.len() as u64;
        let mut failed = 0;
        stack.extend(
            batch
                .drain(saved.len()..)
                .rev()
                .map(|(path, _)| (index, path)),
        );
        for (path, result) in batch.iter().rev() {
            push_initial_children(stack, index, path, result);
        }
        for ((path, result), saved) in batch.into_iter().zip(saved) {
            failed += u64::from(result.is_err());
            if (matches!(saved, ListingSave::Changed)
                && !self.send(Message::Changed {
                    path: path.clone(),
                    refresh_children: None,
                }))
                || !self.send(Message::Listed {
                    path,
                    result,
                    stored: !matches!(saved, ListingSave::Missing),
                })
            {
                return (false, done, failed);
            }
        }
        let alive = self.send(Message::Committed);
        self.wake();
        (alive, done, failed)
    }

    fn store_initial_folder(
        &mut self,
        stack: &mut Vec<(usize, PathBuf)>,
        index: usize,
        path: PathBuf,
    ) -> (bool, u64, u64) {
        let mut failed = false;
        let outcome = self.store(index, &path, RereadMode::Detect, |result| {
            failed = result.is_err();
            push_initial_children(stack, index, &path, result);
        });
        finish_initial_store(outcome, stack, index, path, failed)
    }

    /// 読み始めるものを、深さ優先で後ろから先に積む。DB に残っているドライブは、まだ読み終えて
    /// いないフォルダから（DB の木の並びの順に）。
    fn initial_stack(&self) -> Vec<(usize, PathBuf)> {
        let mut stack = Vec::new();
        for (index, root) in self.roots.iter().enumerate().rev() {
            let todo = match (self.db.as_ref(), &root.drive_id) {
                (Some(db), Some(drive_id)) if root.listed => db
                    .unlisted_folders(drive_id, &root.mount)
                    .unwrap_or_else(|error| {
                        warn!("DB から続きを読めませんでした: {error}");
                        vec![root.mount.clone()]
                    }),
                _ => vec![root.mount.clone()],
            };
            stack.extend(todo.into_iter().rev().map(|path| (index, path)));
        }
        stack
    }

    /// 裏の分を進めてよくなるまで、頼みを済ませながら待つ。UI が閉じたら false。
    fn wait_turn(&mut self, background: &Background) -> bool {
        loop {
            if !self.service_requests() {
                return false;
            }
            self.control.apply_priority();
            if background.rest() == Rest::No {
                return true;
            }
            // 休む間は書き込みの鍵を手放す（ほかのつなぎの名前の変更などを待たせない）。
            if self.db.as_ref().is_some_and(Db::in_transaction) {
                if let Some(Err(error)) = self.db.as_mut().map(Db::commit) {
                    warn!("DB に確定できませんでした: {error}");
                } else if !self.send(Message::Committed) {
                    return false;
                }
            }
            match self.requests.recv_timeout(NAP) {
                Ok(request) => {
                    if !self.handle(request) {
                        return false;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn service_requests(&mut self) -> bool {
        if !self.run_manual_queue() {
            return false;
        }
        while let Ok(request) = self.requests.try_recv() {
            if !self.handle(request) {
                return false;
            }
        }
        if self.pending_visible.is_some() {
            self.retry_visible();
            if self.pending_visible.is_none() {
                return self.hash_visible();
            }
        }
        true
    }

    /// 頼みを済ませる（決まりにかからず、すぐ）。UI が閉じたら false。
    fn handle(&mut self, request: Request) -> bool {
        match request {
            Request::ManualWake => self.run_manual_queue(),
            Request::Visible(path) => {
                // 見ているところは、知らせがあっても今読み直すので、二度は読まない。
                if !self.refresh_hints.contains(&path) {
                    self.hints.remove(&path);
                }
                self.control.urgent.fetch_sub(1, Ordering::Relaxed);
                self.run_manual_queue() && self.visible(&path)
            }
            Request::Refresh(path) => {
                self.control.urgent.fetch_sub(1, Ordering::Relaxed);
                let Some(path) = self.locate(&path) else {
                    return true;
                };
                self.refresh_hints.remove(&path);
                self.hints.remove(&path);
                self.reread_mode(&path, false, RereadMode::Organize)
            }
            Request::Hint(path) => {
                if let Some(path) = self.locate(&path)
                    && !self.is_skipped(&path)
                {
                    self.hints.insert(path);
                }
                true
            }
            Request::DebugForceChanged(path) => {
                let alive = self.reread_forced(&path);
                self.control.urgent.fetch_sub(1, Ordering::Relaxed);
                alive
            }
        }
    }

    /// 見ているフォルダを一段読み直して、直下の仕事を優先する。
    fn visible(&mut self, path: &Path) -> bool {
        if !self.reread(path, false) {
            return false;
        }
        self.promote_visible(path);
        self.hash_visible()
    }

    fn promote_visible(&mut self, path: &Path) {
        self.pending_visible = Some(path.to_path_buf());
        self.retry_visible();
    }

    fn retry_visible(&mut self) {
        let Some(path) = self.pending_visible.take() else {
            return;
        };
        let Some(index) = self.root_of(&path).filter(|_| !self.is_skipped(&path)) else {
            return;
        };
        let root = &self.roots[index];
        let (Some(db), Some(drive_id)) = (&mut self.db, &root.drive_id) else {
            return;
        };
        let result = db
            .find_folder(drive_id, &root.mount, &path)
            .and_then(|folder| {
                if let Some(folder) = folder {
                    db.promote_hash_queue(folder)?;
                }
                Ok(())
            });
        if let Err(error) = result {
            warn!("表示中のハッシュを優先できませんでした: {error}");
            if matches!(
                error.sqlite_error_code(),
                Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
            ) {
                self.pending_visible = Some(path);
            }
        } else {
            self.dirty = true;
        }
    }

    fn hash_visible(&mut self) -> bool {
        let control = self.control.clone();
        let step = |_| {
            !control.manual_pending.load(Ordering::Relaxed)
                && !control.closed.load(Ordering::Relaxed)
        };
        let pace = Pace {
            step: &step,
            ..Pace::FREE
        };
        while step(0) {
            let Some(batch) = self.hash_batch(HASH_VISIBLE, &pace) else {
                break;
            };
            if batch.picked == 0 {
                break;
            }
            let interrupted = batch.interrupted;
            if !self.publish_hash_batch(batch) {
                return false;
            }
            if interrupted {
                self.dirty = true;
                break;
            }
        }
        !control.closed.load(Ordering::Relaxed)
    }

    /// フォルダ `path` を 1 段読み直す（`background` なら I/O を後回しに）。変わっていたら、新しく
    /// できたフォルダの中も読み、あとで裏でハッシュし直す。UI が閉じたら false。
    fn reread(&mut self, path: &Path, background: bool) -> bool {
        self.reread_mode(path, background, RereadMode::Detect)
    }

    fn reread_mode(&mut self, path: &Path, background: bool, mode: RereadMode) -> bool {
        if background {
            self.control.apply_priority();
        } else {
            lisa_core::pace::set_thread_background(false);
        }
        bevy::log::debug!("読み直し: {}", path.display());
        if self.is_skipped(path) {
            return true;
        }
        let Some(index) = self.root_of(path) else {
            return true;
        };
        if self.roots[index].drive_id.is_none()
            || self.db.is_none()
            || !Self::mount_connected(&self.roots[index])
            || !path.is_dir()
        {
            return true;
        }
        let StoreOutcome::Published {
            alive,
            changed,
            first,
        } = self.store_reread(index, path, mode)
        else {
            return true;
        };
        if !alive {
            return false;
        }
        self.dirty |= changed || first;
        if mode == RereadMode::Organize {
            let root = &self.roots[index];
            let already_hashed = self
                .db
                .as_ref()
                .zip(root.drive_id.as_ref())
                .and_then(|(db, drive)| {
                    let id = db.find_folder_ref(drive, &root.mount, path).ok()??;
                    db.entry(id).ok()?.and_then(|entry| entry.hash)
                })
                .is_some();
            if already_hashed {
                let alive = self.send(Message::Hashed {
                    path: path.to_path_buf(),
                });
                self.wake();
                if !alive {
                    return false;
                }
            }
        }

        if !background {
            if changed || first {
                self.hints.insert(path.to_path_buf());
            }
            return true;
        }
        self.fill_unlisted(index, path, background)
    }

    /// 検証用: `reread` と同じ経路で読み直し、`changed = true` として `Changed -> Listed -> Committed` を流す。
    fn reread_forced(&mut self, path: &Path) -> bool {
        lisa_core::pace::set_thread_background(false);
        let Some(path) = self.locate(path) else {
            warn!(
                "DebugForceChanged: ドライブが見つかりません ({})",
                path.display()
            );
            return true;
        };
        let Some(index) = self.root_of(&path) else {
            return true;
        };
        if !path.is_dir() {
            return true;
        }
        !matches!(
            self.store_reread(index, &path, RereadMode::Debug),
            StoreOutcome::Published { alive: false, .. }
        )
    }

    /// `path` の下の、まだ読んでいないフォルダ（読み直しで新しくできたもの）を深さ優先で読む。
    fn fill_unlisted(&mut self, index: usize, path: &Path, background: bool) -> bool {
        let Some(drive_id) = self.roots[index].drive_id.clone() else {
            return true;
        };
        let mount = self.roots[index].mount.clone();
        let mut stack = vec![path.to_path_buf()];
        while let Some(folder) = stack.pop() {
            if self.control.closed.load(Ordering::Relaxed) {
                return false;
            }
            if !self.run_manual_queue() {
                return false;
            }
            let Some(db) = self.db.as_ref() else {
                return true;
            };
            let children = match db
                .find_folder_ref(&drive_id, &mount, &folder)
                .ok()
                .flatten()
                .map(|id| db.children(id))
            {
                Some(Ok(children)) => children,
                _ => continue,
            };
            for child in children.iter().rev() {
                let readable =
                    child.kind == listing::Kind::Dir && !child.cloud_only && !child.other_volume;
                if !readable {
                    continue;
                }
                let child_path = folder.join(lisa_core::name::from_raw_bytes(&child.name_raw));
                if self.is_skipped(&child_path) {
                    continue;
                }
                if !child.listed {
                    if background {
                        self.control.apply_priority();
                    }
                    match self.store_reread(index, &child_path, RereadMode::Detect) {
                        StoreOutcome::Published { alive: false, .. } => return false,
                        StoreOutcome::Published { .. } => stack.push(child_path),
                        StoreOutcome::Busy => {}
                    }
                }
            }
        }
        true
    }

    fn hash_batch(&mut self, max_priority: i64, pace: &Pace) -> Option<hash::HashBatch> {
        let db = self.db.as_mut()?;
        let mounts: Vec<_> = self
            .roots
            .iter()
            .filter(|root| Self::mount_connected(root))
            .filter_map(|root| Some((root.drive_id.as_deref()?, root.mount.as_path())))
            .collect();
        match hash::hash_queue_batch_checked(db, &mounts, max_priority, pace, |drive, mount| {
            self.roots.iter().any(|root| {
                root.drive_id.as_deref() == Some(drive)
                    && root.mount == mount
                    && Self::mount_connected(root)
            })
        }) {
            Ok(batch) => Some(batch),
            Err(error) => {
                warn!("ハッシュを DB に書けませんでした: {error}");
                self.hash_retry_needed = true;
                None
            }
        }
    }

    fn publish_hash_batch(&mut self, batch: hash::HashBatch) -> bool {
        report(&self.progress, |p| {
            if let Some((path, _)) = batch.files.last() {
                p.hash.folder = path.parent().map(Path::to_path_buf);
            }
            p.hash.files += batch.files.len() as u64;
            p.hash.bytes += batch.files.iter().map(|(_, bytes)| bytes).sum::<u64>();
        });
        for hint in batch.hints {
            if let Some(root) = self.root_of(&hint).map(|index| &self.roots[index])
                && Self::mount_connected(root)
                && let Some(parent) = hint
                    .ancestors()
                    .take_while(|path| path.starts_with(&root.mount))
                    .find(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()))
            {
                self.hints.insert(parent.to_path_buf());
            }
        }
        self.update_hash_queue_status(false);
        for path in batch.folders {
            if !self.send(Message::Hashed { path }) {
                return false;
            }
        }
        let alive = self.send(Message::Committed);
        self.wake();
        alive
    }

    fn store_reread(&mut self, index: usize, path: &Path, mode: RereadMode) -> StoreOutcome {
        let outcome = self.store(index, path, mode, |_| {});
        if matches!(outcome, StoreOutcome::Busy) {
            self.hints.insert(path.to_path_buf());
            if mode == RereadMode::Organize {
                self.refresh_hints.insert(path.to_path_buf());
            }
        }
        outcome
    }

    /// フォルダを読み、結果を通知する。Busy の場合は呼び出し元が再度キューに積む。
    fn store(
        &mut self,
        index: usize,
        path: &Path,
        mode: RereadMode,
        on_read: impl FnOnce(&Result<listing::Listing, String>),
    ) -> StoreOutcome {
        let root = &self.roots[index];
        let mut stored = false;
        let mut changed = mode != RereadMode::Detect;
        let mut first = false;
        let mut retries = 0;
        let result = loop {
            let version = match (&self.db, &root.drive_id) {
                (Some(db), Some(_)) => db.listing_version().map(Some),
                _ => Ok(None),
            };
            let mut result = Self::read_dir_filtered(&self.skip_dirs, path);
            match (self.db.as_mut(), &root.drive_id, version) {
                (Some(db), Some(drive_id), Ok(Some(version))) => {
                    let mut saved =
                        db.save_listing_at(drive_id, &root.mount, path, &result, Some(version));
                    if matches!(saved, Ok(ListingSave::Retry)) {
                        if retries < 2 {
                            retries += 1;
                            continue;
                        }
                        // 3 回続けて競合したときだけ、他の確定で待ち続けないよう書き込み権を取って読み直す。
                        match db.save_fresh_listing(drive_id, &root.mount, path, || {
                            Self::read_dir_filtered(&self.skip_dirs, path)
                        }) {
                            Ok((fresh_saved, fresh_result)) => {
                                saved = Ok(fresh_saved);
                                result = fresh_result;
                            }
                            Err(error) => saved = Err(error),
                        }
                    }
                    match saved {
                        Ok(saved) => {
                            stored = !matches!(saved, ListingSave::Missing);
                            changed |= matches!(saved, ListingSave::Changed);
                            first = matches!(saved, ListingSave::First);
                        }
                        Err(error) => {
                            warn!("DB に書けませんでした（{}）: {error}", path.display());
                            if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) {
                                return StoreOutcome::Busy;
                            }
                        }
                    }
                }
                (_, _, Err(error)) => {
                    warn!(
                        "DB の更新状態を読めませんでした（{}）: {error}",
                        path.display()
                    );
                    if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) {
                        return StoreOutcome::Busy;
                    }
                }
                _ => {}
            }
            break result;
        };
        on_read(&result);
        // 変わっていたら、先に知らせる（UI は木の子を出し直し、古いにする）。
        let alive = (!changed
            || self.send(Message::Changed {
                path: path.to_path_buf(),
                refresh_children: (mode == RereadMode::Organize).then(|| {
                    result
                        .as_ref()
                        .ok()
                        .into_iter()
                        .flat_map(|listing| &listing.entries)
                        .filter(|entry| super::readable(entry))
                        .map(|entry| path.join(&entry.name))
                        .collect()
                }),
            }))
            && self.send(Message::Listed {
                path: path.to_path_buf(),
                result,
                stored,
            })
            && (!stored || self.send(Message::Committed));
        self.wake();
        StoreOutcome::Published {
            alive,
            changed,
            first,
        }
    }

    /// 知らせのパスを、読むドライブの下のパスにする。Mac の FSEvents は起動ディスクの Data のボリュームの
    /// 中を `/Users/…` のように firmlink の側で知らせるので、`/System/Volumes/Data` を前に付けて探す。
    /// どのドライブの下でもなければ None。
    fn locate(&self, path: &Path) -> Option<PathBuf> {
        if self.root_of(path).is_some() {
            return Some(path.to_path_buf());
        }
        let relative = path.strip_prefix("/").ok()?;
        self.roots
            .iter()
            .filter(|r| r.drive_id.is_some())
            .map(|r| r.mount.join(relative))
            .find(|p| self.root_of(p).is_some() && p.exists())
    }

    /// `path` があるドライブ（つないでいる場所がいちばん長く合うもの）。
    fn root_of(&self, path: &Path) -> Option<usize> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, r)| path.starts_with(&r.mount))
            .max_by_key(|(_, r)| r.mount.components().count())
            .map(|(i, _)| i)
    }

    /// 隔離フォルダ（`.lisa-trash` と、データのフォルダの `trash/`）か。
    fn is_skipped(&self, path: &Path) -> bool {
        Self::path_is_skipped(&self.skip_dirs, path)
    }

    fn path_is_skipped(skip_dirs: &[PathBuf], path: &Path) -> bool {
        if path
            .components()
            .any(|c| lisa_core::name::is_quarantine_dir(c.as_os_str()))
        {
            return true;
        }
        skip_dirs.iter().any(|s| path == s || path.starts_with(s))
    }

    /// フォルダを 1 段読み、飛ばす対象（隔離フォルダ）を除いて返す。
    fn read_dir_filtered(skip_dirs: &[PathBuf], path: &Path) -> Result<listing::Listing, String> {
        let mut listing = listing::read_dir(path).map_err(|e| e.to_string())?;
        if !skip_dirs.is_empty() {
            listing
                .entries
                .retain(|e| !Self::path_is_skipped(skip_dirs, &path.join(&e.name)));
        }
        Ok(listing)
    }

    fn send(&self, message: Message) -> bool {
        self.sender.send(message).is_ok()
    }

    /// 止まっている間でも、すぐに木に足して描けるように起こす。
    fn wake(&self) {
        if let Some(proxy) = &self.proxy {
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 整理の読み直しは一覧が同じでも通知してハッシュ処理を起こす() {
        let temp = crate::preview::TestTemp::new();
        let mount = temp.0.join("volume");
        let child = mount.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let drive = "test:organize-refresh";
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("test", "test", "test").unwrap();
        db.upsert_drive(
            &device,
            &lisa_core::volumes::Volume {
                id: Some(drive.into()),
                mount_path: mount.clone(),
                ..super::super::root_volume()
            },
        )
        .unwrap();
        let listing = listing::read_dir(&mount).map_err(|err| err.to_string());
        db.save_listing(drive, &mount, &mount, &listing).unwrap();
        assert!(matches!(
            db.save_listing_at(drive, &mount, &mount, &listing, None)
                .unwrap(),
            ListingSave::Unchanged
        ));

        let (sender, results) = std::sync::mpsc::channel();
        let (_send, requests) = std::sync::mpsc::channel();
        let control = Arc::new(Control::default());
        control.urgent.store(1, Ordering::Relaxed);
        let mut worker = Worker {
            db: Some(db),
            roots: vec![Root {
                node: 0,
                drive_id: Some(drive.into()),
                mount: mount.clone(),
                mount_identity: lisa_core::volumes::mount_identity(&mount),
                listed: true,
            }],
            sender,
            requests,
            proxy: None,
            progress: Arc::new(Mutex::new(Progress::default())),
            control: control.clone(),
            dirty: false,
            hash_retry_needed: false,
            hash_stats_at: None,
            pending_visible: None,
            hints: Default::default(),
            refresh_hints: Default::default(),
            skip_dirs: vec![],
        };
        assert!(worker.handle(Request::Refresh(mount.clone())));
        assert!(worker.dirty);
        assert_eq!(control.urgent.load(Ordering::Relaxed), 0);
        let sent = results.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            sent.as_slice(),
            [
                Message::Changed { path: changed, refresh_children: Some(children) },
                Message::Listed { path: listed, stored: true, .. },
                Message::Committed
            ] if changed == &mount && listed == &mount && children == &[child]
        ));
    }

    #[test]
    fn busy_initial_folder_remains_pending_without_counting_as_finished() {
        let folder = PathBuf::from("root");
        let mut stack = vec![];
        assert_eq!(
            finish_initial_store(StoreOutcome::Busy, &mut stack, 3, folder.clone(), false),
            (true, 0, 0)
        );
        assert_eq!(stack.pop(), Some((3, folder)));
    }
}

#[cfg(test)]
mod boost_tests {
    use super::*;

    #[test]
    fn ハッシュのcpu待機中に終了と中止を見直す() {
        for close in [true, false] {
            let control = Arc::new(Control::default());
            let background = Background::new(control.clone());
            *background.checked.lock().unwrap() = RestChecks {
                sampled: Some(Instant::now()),
                cpu: true,
                ..Default::default()
            };
            let checks = std::cell::Cell::new(0);
            assert!(!background.step(1, || {
                let count = checks.get() + 1;
                checks.set(count);
                if close {
                    control.closed.store(true, Ordering::Relaxed);
                }
                count == 2
            }));
            assert_eq!(control.rest(), Rest::Cpu);
            assert_eq!(checks.get(), if close { 1 } else { 2 });
        }
    }

    #[test]
    fn 優先度の失敗は現在の世代に一度だけ記録する() {
        let control = Control::default();
        control.set_active(true);
        let revision = control.mode.load(Ordering::Relaxed) & !MODE_FLAGS;
        assert!(control.mark_priority_failure(revision));
        assert!(!control.mark_priority_failure(revision));
        control.set_boost(true);
        assert!(!control.mark_priority_failure(revision));
        assert_eq!(control.boost_status(), (true, true, false));
    }

    #[test]
    fn ブーストは作業中だけ切り替えられ次の作業へ持ち越さない() {
        let control = Control::default();
        control.set_boost(true);
        assert_eq!(control.boost_status(), (false, false, false));
        control.set_active(true);
        control.set_boost(true);
        let mode = control.mode.load(Ordering::Relaxed) | PRIORITY_FAILED;
        control.mode.store(mode, Ordering::Relaxed);
        control.set_boost(true);
        assert_eq!(control.mode.load(Ordering::Relaxed), mode);
        control.set_boost(false);
        assert_eq!(control.boost_status(), (true, false, false));
        control.set_boost(true);
        control.set_active(false);
        control.set_active(true);
        assert_eq!(control.boost_status(), (true, false, false));
    }

    #[test]
    fn ブーストはキャッシュしたディスク待機を残しcpu待機だけを除外する() {
        let control = Arc::new(Control::default());
        control.set_active(true);
        let background = Background::new(control.clone());
        *background.checked.lock().unwrap() = RestChecks {
            sampled: Some(Instant::now()),
            cpu_revision: control.mode.load(Ordering::Relaxed) & !MODE_FLAGS,
            cpu: true,
            disk: true,
            ..Default::default()
        };
        assert_eq!(background.rest(), Rest::Cpu);
        control.set_boost(true);
        assert_eq!(background.rest(), Rest::Disk);
        control.paused.store(true, Ordering::Relaxed);
        assert_eq!(background.rest(), Rest::Paused);
        control.paused.store(false, Ordering::Relaxed);
        control.set_boost(false);
        // 最新の CPU しきい値は 100 なので、以前の CPU 待機に戻らない。
        assert_eq!(background.rest(), Rest::Disk);
    }
}
