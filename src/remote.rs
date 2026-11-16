//! 外（`lisa-mcp` など）からの頼みを受けて、画面を動かす（`lisa_core::remote`）。
//!
//! 起動したら `127.0.0.1` で待ち、番号と合言葉を DB と同じフォルダの `control.json` に書く。頼みは
//! 待つ係のスレッドから届くので、届いたらアプリを起こし、毎フレーム `handle` で受け取って返事をする。
//! 頼めるのは、今の様子を聞く、場所へ飛ぶ（`scan::JumpTo`）、重複の一覧の開け閉めと組を選ぶ、
//! フォルダを先に読み直す、一時停止だけ。ファイルは動かさない。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::remote::{self, Command, Incoming, Reply, State};

use crate::dups::{Dups, DupsState};
use crate::model::tree::FileTree;
use crate::scan::{JumpTo, ScanControl, ScanProgress};
use crate::view::Nav;

pub struct RemotePlugin;

impl Plugin for RemotePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, start)
            .add_systems(Update, (handle, select_pending).chain())
            .add_systems(Last, remove_on_exit);
    }
}

/// 届いた頼みを受け取る口と、書いた `control.json` の場所。
#[derive(Resource)]
struct Inbox {
    receiver: Mutex<Receiver<Incoming>>,
    file: PathBuf,
}

impl Drop for Inbox {
    fn drop(&mut self) {
        remote::remove(&self.file);
    }
}

/// 一覧ができたら選ぶ組（頼まれたときにまだ一覧が無かったもの）。
#[derive(Resource)]
struct PendingSelect(String);

fn start(mut commands: Commands, proxy: Res<EventLoopProxyWrapper>) {
    let Some(dir) = crate::scan::db_path().and_then(|p| p.parent().map(PathBuf::from)) else {
        return;
    };
    let (sender, receiver) = channel();
    let proxy = (**proxy).clone();
    let deliver = move |incoming: Incoming| {
        let sent = sender.send(incoming).is_ok();
        // 止まっている間でも、すぐ受け取れるように起こす。
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
        sent
    };
    match remote::listen(&dir, deliver) {
        Ok(file) => {
            info!("外からの頼みを待っています（{}）", file.display());
            commands.insert_resource(Inbox {
                receiver: Mutex::new(receiver),
                file,
            });
        }
        Err(error) => warn!("外からの頼みを待てませんでした: {error}"),
    }
}

/// 終わるときに `control.json` を消す。
fn remove_on_exit(mut exit: MessageReader<AppExit>, inbox: Option<Res<Inbox>>) {
    if exit.read().next().is_some()
        && let Some(inbox) = inbox
    {
        remote::remove(&inbox.file);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle(
    mut commands: Commands,
    inbox: Option<Res<Inbox>>,
    nav: Res<Nav>,
    tree: Res<FileTree>,
    mut dups: ResMut<Dups>,
    control: Option<Res<ScanControl>>,
    progress: Option<Res<ScanProgress>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Some(inbox) = inbox else {
        return;
    };
    let incoming: Vec<Incoming> = match inbox.receiver.lock() {
        Ok(receiver) => receiver.try_iter().collect(),
        Err(_) => return,
    };
    for Incoming { command, reply } in incoming {
        let result: Result<(), String> = match command {
            Command::State => Ok(()),
            Command::Show { chain } => {
                if chain.is_empty() {
                    Err("行き先が空です".into())
                } else {
                    commands.insert_resource(JumpTo(chain));
                    Ok(())
                }
            }
            Command::Dups { open, select } => {
                dups.open = open;
                if !open {
                    dups.selected = None;
                }
                if let (true, Some(hash)) = (open, select) {
                    // 一覧がまだなら、できてから選ぶ（`select_pending`）。
                    match index_of(&dups, &hash) {
                        Some(i) => dups.selected = Some(i),
                        None => commands.insert_resource(PendingSelect(hash)),
                    }
                }
                Ok(())
            }
            Command::Rescan { path } => match &control {
                Some(control) if path.is_dir() => {
                    control.visible(path);
                    Ok(())
                }
                Some(_) => Err(format!("{} はフォルダではありません", path.display())),
                None => Err("読む係がまだ動いていません".into()),
            },
            Command::Pause { paused } => match &control {
                Some(control) => {
                    control.control.paused.store(paused, Ordering::Relaxed);
                    Ok(())
                }
                None => Err("読む係がまだ動いていません".into()),
            },
        };
        let answer = match result {
            Ok(()) => Reply::ok(state(
                &nav,
                &tree,
                &dups,
                control.as_deref(),
                progress.as_deref(),
            )),
            Err(message) => Reply::error(message),
        };
        let _ = reply.send(answer);
        redraw.write(RequestRedraw);
    }
}

/// 頼まれた組を、一覧ができたら選ぶ。一覧を閉じたら諦める。
fn select_pending(
    mut commands: Commands,
    pending: Option<Res<PendingSelect>>,
    mut dups: ResMut<Dups>,
) {
    let Some(pending) = pending else {
        return;
    };
    if !dups.open {
        commands.remove_resource::<PendingSelect>();
        return;
    }
    if let Some(i) = index_of(&dups, &pending.0) {
        dups.selected = Some(i);
        commands.remove_resource::<PendingSelect>();
    } else if matches!(dups.state, DupsState::Ready(_) | DupsState::Failed(_)) {
        warn!("頼まれた重複の組が一覧にありません");
        commands.remove_resource::<PendingSelect>();
    }
}

/// 一覧の中の、ハッシュ（16 進）が `hash` の組の番号。
fn index_of(dups: &Dups, hash: &str) -> Option<usize> {
    let hash = hash.trim().to_ascii_lowercase();
    dups.groups()
        .iter()
        .position(|g| lisa_core::query::hex(&g.hash) == hash)
}

/// 今の様子。
fn state(
    nav: &Nav,
    tree: &FileTree,
    dups: &Dups,
    control: Option<&ScanControl>,
    progress: Option<&ScanProgress>,
) -> State {
    let scene = crate::view::scene_path(nav, tree);
    let folder = scene[scene.len() - 1].node;
    let mut state = State {
        folder: tree.nodes[folder].path.clone(),
        breadcrumb: scene
            .iter()
            .map(|l| tree.nodes[l.node].name.clone())
            .collect(),
        entry: tree.nodes[folder].entry,
        dups_open: dups.open,
        dups_state: match dups.state {
            DupsState::NotYet => "not_yet",
            DupsState::Computing { .. } => "computing",
            DupsState::Ready(_) => "ready",
            DupsState::Failed(_) => "failed",
        }
        .into(),
        dups_groups: dups.groups().len(),
        dups_selected: dups
            .selected
            .and_then(|i| dups.groups().get(i))
            .map(|g| lisa_core::query::hex(&g.hash)),
        paused: control.is_some_and(|c| c.control.paused.load(Ordering::Relaxed)),
        ..default()
    };
    if let Some(progress) = progress.and_then(|p| p.0.lock().ok()) {
        state.folders_read = progress.done;
        state.folders_failed = progress.failed;
        state.folders_queued = progress.queued;
        state.reading = progress.current.clone();
        state.files_hashed = progress.hash.files;
        state.bytes_hashed = progress.hash.bytes;
        state.hashing = progress.hash.folder.clone();
        state.finished = progress.finished.is_some();
    }
    state
}
