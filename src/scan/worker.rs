//! 読む係（裏のスレッド 1 本）。ドライブを深さ優先で読み、読み終えたらハッシュし、そのあとは頼みを
//! 待つ。`docs/scan.md` の「裏で読む分」の決まりに従う:
//!
//! - 見ているところ（開いたフォルダ）の頼み（`Request::Visible`）が来たら、裏の分はすぐやめて
//!   （ハッシュは 1 MiB ごとに合図を見る）、先にそれを 1 段読み直す。DB と違えば DB を直し、変わった
//!   ものとそのフォルダ・祖先のハッシュを消して（古い）、ハッシュし直す。これは決まりにかからない。
//! - 裏の分は I/O を後回しにし（`pace::set_thread_background`）、速さの上限（設定）を守る。
//! - 一時停止、電池で動いているとき、CPU が混んでいるとき（設定のしきい値）は休む。
//! - 監視（`Request::Hint`、FSEvents など）で変わったと知らされたフォルダは、裏の分として（上の
//!   決まりに従って）読み直す。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::log::warn;
use bevy::winit::EventLoopProxy;
use bevy::winit::WinitUserEvent;
use lisa_core::db::Db;
use lisa_core::hash::{self, Pace};
use lisa_core::listing;

use super::{Message, Progress, Root, SHOWN_NEXT, readable, report};

/// DB に書いたものを確定する間隔。
const COMMIT_EVERY: Duration = Duration::from_millis(300);
/// ファイルを同時にハッシュする数。
const HASH_THREADS: usize = 4;
/// 休む・止めるかを見直す間隔。
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// 休んでいるあいだ、合図を見に行く間隔。
const NAP: Duration = Duration::from_millis(250);

/// UI から読む係への頼み。
pub enum Request {
    /// このフォルダを今見ている（1 段読み直し、ハッシュの無いものをハッシュする）。
    Visible(PathBuf),
    /// このフォルダの中が変わったらしい（監視からの知らせ。裏の分として読み直す）。
    Hint(PathBuf),
}

/// 休んでいる理由（`Control::resting`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rest {
    No = 0,
    Paused = 1,
    Battery = 2,
    Cpu = 3,
}

/// UI と読む係で分け合う、裏で読むときの決まり。
pub struct Control {
    /// 一時停止（メニューのボタン）。
    pub paused: AtomicBool,
    /// まだ済んでいない見ているところの頼みの数。0 でなければ、裏の分はやめる。
    pub urgent: AtomicUsize,
    /// 裏の分の速さの上限（バイト毎秒、0 なら上限なし）。
    pub limit: AtomicU64,
    /// 電池で動いているときは休む。
    pub rest_on_battery: AtomicBool,
    /// CPU の混み具合（%）がこれを超えたら休む（100 なら休まない）。
    pub cpu_limit: AtomicU32,
    /// 今休んでいる理由（`Rest`）。
    pub resting: AtomicU8,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            urgent: AtomicUsize::new(0),
            limit: AtomicU64::new(0),
            rest_on_battery: AtomicBool::new(true),
            cpu_limit: AtomicU32::new(100),
            resting: AtomicU8::new(Rest::No as u8),
        }
    }
}

impl Control {
    pub fn rest(&self) -> Rest {
        match self.resting.load(Ordering::Relaxed) {
            1 => Rest::Paused,
            2 => Rest::Battery,
            3 => Rest::Cpu,
            _ => Rest::No,
        }
    }
}

/// 裏の分の読み方（速さの上限と、休むかどうか）。
struct Background {
    control: Arc<Control>,
    /// 速さの上限を守るための、この秒の始まりと、その秒に読んだ量。
    window: Mutex<(Instant, u64)>,
    /// 最後に電池と CPU を見た時刻と、そのときの答え。
    checked: Mutex<(Option<Instant>, Rest)>,
}

impl Background {
    fn new(control: Arc<Control>) -> Self {
        Self {
            control,
            window: Mutex::new((Instant::now(), 0)),
            checked: Mutex::new((None, Rest::No)),
        }
    }

    /// 今休むべきか（理由）。電池と CPU は `CHECK_EVERY` ごとに見直す。
    fn rest(&self) -> Rest {
        let c = &self.control;
        let rest = if c.paused.load(Ordering::Relaxed) {
            Rest::Paused
        } else {
            let mut checked = self.checked.lock().expect("休むかどうか");
            if checked.0.is_none_or(|t| t.elapsed() >= CHECK_EVERY) {
                let battery =
                    c.rest_on_battery.load(Ordering::Relaxed) && lisa_core::pace::on_battery();
                let limit = c.cpu_limit.load(Ordering::Relaxed);
                let cpu = limit < 100
                    && lisa_core::pace::cpu_load_percent().is_some_and(|p| p > limit as f32);
                *checked = (
                    Some(Instant::now()),
                    if battery {
                        Rest::Battery
                    } else if cpu {
                        Rest::Cpu
                    } else {
                        Rest::No
                    },
                );
            }
            checked.1
        };
        c.resting.store(rest as u8, Ordering::Relaxed);
        rest
    }

    /// 見ているところの頼みが待っている。
    fn urgent(&self) -> bool {
        self.control.urgent.load(Ordering::Relaxed) > 0
    }

    /// 裏の分を続けてよいか。`bytes` だけ読んだので、速さの上限を超えていれば待つ。
    fn step(&self, bytes: u64) -> bool {
        if self.urgent() || self.rest() != Rest::No {
            return false;
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
        !self.urgent()
    }
}

/// 読む係が持つもの。
pub(super) struct Worker {
    pub db: Option<Db>,
    pub roots: Vec<Root>,
    pub sender: Sender<Message>,
    pub requests: Receiver<Request>,
    pub proxy: EventLoopProxy<WinitUserEvent>,
    pub progress: Arc<Mutex<Progress>>,
    pub control: Arc<Control>,
    /// 読み直しでハッシュを消したので、裏でドライブ全体のハッシュをやり直す（祖先を作り直す）。
    pub dirty: bool,
    /// 監視から知らされた、読み直すフォルダ（同じものは 1 つにまとめる）。
    pub hints: std::collections::BTreeSet<PathBuf>,
}

impl Worker {
    pub fn run(mut self) {
        let background = Background::new(self.control.clone());
        let mut stack = self.initial_stack();
        if stack.is_empty() {
            report(&self.progress, |p| p.finished = Some(Instant::now()));
        }
        // 1. 深さ優先で読む。
        let mut last_commit = Instant::now();
        while let Some((index, path)) = stack.pop() {
            if !self.wait_turn(&background) {
                return;
            }
            lisa_core::pace::set_thread_background(true);
            report(&self.progress, |p| {
                p.current = Some(path.clone());
                p.queued = stack.len();
            });
            let result = listing::read_dir(&path).map_err(|e| e.to_string());
            if let Ok(listing) = &result {
                for entry in listing.entries.iter().rev().filter(|e| readable(e)) {
                    stack.push((index, path.join(&entry.name)));
                }
            }
            let due = stack.is_empty() || last_commit.elapsed() >= COMMIT_EVERY;
            if due {
                last_commit = Instant::now();
            }
            let failed = result.is_err();
            let (alive, _) = self.store(index, path, result, due);
            report(&self.progress, |p| {
                p.done += 1;
                p.failed += failed as u64;
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
        // 2. ハッシュの無いものを、下からハッシュする。
        if self.db.is_some() {
            report(&self.progress, |p| p.hash.started = Some(Instant::now()));
            if !self.hash_all(&background) {
                return;
            }
            report(&self.progress, |p| {
                p.hash.folder = None;
                p.hash.finished = Some(Instant::now());
            });
            self.wake();
        }
        // 3. あとは頼みを待つ。監視の知らせは裏の分として読み直し、ハッシュを消したら、裏で祖先を
        //    作り直す。
        loop {
            while !self.hints.is_empty() {
                if !self.wait_turn(&background) {
                    return;
                }
                let Some(path) = self.hints.pop_first() else {
                    break;
                };
                if !self.reread(&path, true) {
                    return;
                }
            }
            if self.dirty && self.db.is_some() {
                self.dirty = false;
                if !self.hash_all(&background) {
                    return;
                }
                self.wake();
            }
            // 知らせが続くあいだは、少しまとめてから読み直す。
            let wait = if self.hints.is_empty() {
                Duration::from_secs(60)
            } else {
                NAP
            };
            match self.requests.recv_timeout(wait) {
                Ok(request) => {
                    if !self.handle(request) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// 全部のドライブの、ハッシュの無いものを裏の分としてハッシュする（やめさせられたら、頼みを
    /// 済ませてやり直す）。UI が閉じたら false。
    fn hash_all(&mut self, background: &Background) -> bool {
        for index in 0..self.roots.len() {
            loop {
                if !self.wait_turn(background) {
                    return false;
                }
                let interrupted = AtomicBool::new(false);
                let step = |bytes: u64| {
                    let go = background.step(bytes);
                    if !go {
                        interrupted.store(true, Ordering::Relaxed);
                    }
                    go
                };
                let pace = Pace {
                    background: true,
                    step: &step,
                };
                if !self.hash(index, None, &pace) {
                    return false;
                }
                if !interrupted.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
        true
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
            // 待っている頼みを先に済ませる。
            while let Ok(request) = self.requests.try_recv() {
                if !self.handle(request) {
                    return false;
                }
            }
            if background.rest() == Rest::No {
                return true;
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

    /// 頼みを済ませる（決まりにかからず、すぐ）。UI が閉じたら false。
    fn handle(&mut self, request: Request) -> bool {
        match request {
            Request::Visible(path) => {
                // 見ているところは、知らせがあっても今読み直すので、二度は読まない。
                self.hints.remove(&path);
                let alive = self.visible(&path);
                self.control.urgent.fetch_sub(1, Ordering::Relaxed);
                alive
            }
            Request::Hint(path) => {
                self.hints.insert(path);
                true
            }
        }
    }

    /// 見ているフォルダ `path` を 1 段読み直し（下は、裏の深さ優先がたどる）、ハッシュの無いものを
    /// ハッシュする。変わっていたら、裏で祖先のハッシュも作り直す。
    fn visible(&mut self, path: &Path) -> bool {
        if !self.reread(path, false) {
            return false;
        }
        let Some(index) = self.root_of(path) else {
            return true;
        };
        self.hash(index, Some(path), &Pace::FREE)
    }

    /// フォルダ `path` を 1 段読み直す（`background` なら I/O を後回しに）。変わっていたら、新しく
    /// できたフォルダの中も読み、あとで裏でハッシュし直す。UI が閉じたら false。
    fn reread(&mut self, path: &Path, background: bool) -> bool {
        lisa_core::pace::set_thread_background(background);
        let Some(index) = self.root_of(path) else {
            return true;
        };
        if self.roots[index].drive_id.is_none() || self.db.is_none() || !path.is_dir() {
            return true;
        }
        let result = listing::read_dir(path).map_err(|e| e.to_string());
        let (alive, changed) = self.store(index, path.to_path_buf(), result, true);
        if !alive {
            return false;
        }
        if changed {
            self.dirty = true;
            if !self.fill_unlisted(index, path) {
                return false;
            }
        }
        true
    }

    /// `path` の下の、まだ読んでいないフォルダ（読み直しで新しくできたもの）を深さ優先で読む。
    fn fill_unlisted(&mut self, index: usize, path: &Path) -> bool {
        let Some(drive_id) = self.roots[index].drive_id.clone() else {
            return true;
        };
        let mount = self.roots[index].mount.clone();
        let mut stack = vec![path.to_path_buf()];
        while let Some(folder) = stack.pop() {
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
                if !child.listed {
                    let result = listing::read_dir(&child_path).map_err(|e| e.to_string());
                    if !self.store(index, child_path.clone(), result, true).0 {
                        return false;
                    }
                    stack.push(child_path);
                }
            }
        }
        true
    }

    /// ドライブ `index` の、`folder`（無ければ全体）のハッシュの無いものをハッシュする。UI が閉じたら false。
    fn hash(&mut self, index: usize, folder: Option<&Path>, pace: &Pace) -> bool {
        let Some(drive_id) = self.roots[index].drive_id.clone() else {
            return true;
        };
        let mount = self.roots[index].mount.clone();
        let Some(db) = self.db.as_mut() else {
            return true;
        };
        let closed = AtomicBool::new(false);
        let (sender, proxy, progress) = (&self.sender, &self.proxy, &self.progress);
        let mut on = |event: hash::Event| {
            let message = match event {
                hash::Event::Folder(path) => {
                    report(progress, |p| p.hash.folder = Some(path.to_path_buf()));
                    return;
                }
                hash::Event::File { bytes } => {
                    report(progress, |p| {
                        p.hash.files += 1;
                        p.hash.bytes += bytes;
                    });
                    return;
                }
                hash::Event::FolderDone(path) => Message::Hashed {
                    path: path.to_path_buf(),
                },
                hash::Event::Committed => Message::Committed,
            };
            if sender.send(message).is_err() {
                closed.store(true, Ordering::Relaxed);
            }
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        };
        let step = |bytes| !closed.load(Ordering::Relaxed) && (pace.step)(bytes);
        let pace = Pace {
            background: pace.background,
            step: &step,
        };
        let result = match folder {
            Some(path) => {
                hash::hash_folder(db, &drive_id, &mount, path, HASH_THREADS, &mut on, &pace)
            }
            None => hash::hash_drive(db, &drive_id, &mount, HASH_THREADS, &mut on, &pace),
        };
        if let Err(error) = result {
            warn!("ハッシュを DB に書けませんでした: {error}");
        }
        !closed.load(Ordering::Relaxed)
    }

    /// 読んだ結果を DB に書き（`commit` なら確定もし）、UI に知らせる。（UI がまだいるか、読み直しで
    /// DB と違っていたか）を返す。
    fn store(
        &mut self,
        index: usize,
        path: PathBuf,
        result: Result<listing::Listing, String>,
        commit: bool,
    ) -> (bool, bool) {
        let root = &self.roots[index];
        let mut stored = false;
        let mut committed = false;
        let mut changed = false;
        if let (Some(db), Some(drive_id)) = (self.db.as_mut(), &root.drive_id) {
            match db
                .begin()
                .and_then(|()| db.save_listing(drive_id, &root.mount, &path, &result))
            {
                Ok(c) => {
                    stored = true;
                    changed = c;
                }
                Err(error) => warn!("DB に書けませんでした（{}）: {error}", path.display()),
            }
            if commit {
                match db.commit() {
                    Ok(()) => committed = true,
                    Err(error) => warn!("DB に確定できませんでした: {error}"),
                }
            }
        }
        // 変わっていたら、先に知らせる（UI は木の子を出し直し、古いにする）。
        let alive = (!changed || self.send(Message::Changed { path: path.clone() }))
            && self.send(Message::Listed {
                path,
                result,
                stored,
            })
            && (!committed || self.send(Message::Committed));
        self.wake();
        (alive, changed)
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

    fn send(&self, message: Message) -> bool {
        self.sender.send(message).is_ok()
    }

    /// 止まっている間でも、すぐに木に足して描けるように起こす。
    fn wake(&self) {
        let _ = self.proxy.send_event(WinitUserEvent::WakeUp);
    }
}
