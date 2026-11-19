//! 重複の一覧（`lisa_core::dups`）を裏で作り、選んだ組のコピーを木の上で光らせる。
//! 表示だけで、ファイルは何も動かさない。
//!
//! 一覧は、画面の横の一覧（`ui/dups.rs`）を開くたびに、裏のスレッドで確かめる。作った一覧は、DB の状態
//! （ドライブごとの一番上のハッシュ、`dups::signature`）と一緒にキャッシュ（`duplicates.json`）に残し、
//! 状態が同じならそれをすぐ出す。違えば、前の一覧を出したまま作り直し、できたら差し替える。
//! 選んだ組のコピーは、木にあれば太い枠（`DupHighlight::copies`）、まだ木に出していなければ、
//! 出してある一番深い祖先に細い枠（`around`）を付ける（`render/scene.rs` の `mark_duplicate`）。

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use lisa_core::dups::Group;

use crate::model::tree::{FileTree, NodeId};
use crate::view::SyncRequest;

/// 一覧に出す組の数の上限。
const MAX_GROUPS: usize = 300;

pub struct DupsPlugin;

impl Plugin for DupsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Dups>()
            .init_resource::<DupHighlight>()
            .add_systems(Update, (compute, update_highlight).chain());
    }
}

/// 重複の一覧の状態。
#[derive(Resource, Default)]
pub struct Dups {
    /// 横の一覧を開いている。
    pub open: bool,
    pub state: DupsState,
    /// 選んだ組（`state` の一覧の番号）。
    pub selected: Option<usize>,
}

#[derive(Default)]
pub enum DupsState {
    /// まだ作っていない。
    #[default]
    NotYet,
    /// 裏で確かめている・作っている（`previous` はそのあいだ見せておく前の一覧）。
    Computing {
        previous: Option<Vec<Group>>,
        receiver: Mutex<Receiver<Found>>,
    },
    Ready(Vec<Group>),
    Failed(String),
}

impl Dups {
    pub fn groups(&self) -> &[Group] {
        match &self.state {
            DupsState::Ready(groups) => groups,
            DupsState::Computing {
                previous: Some(groups),
                ..
            } => groups,
            _ => &[],
        }
    }
}

/// 裏のスレッドから届くもの。
pub enum Found {
    /// キャッシュにあった前の一覧（このあと作り直したものが届く）。
    Previous(Vec<Group>),
    Done(Result<Vec<Group>, String>),
}

/// キャッシュの場所。
fn cache_path() -> Option<std::path::PathBuf> {
    crate::scan::cache_dir().map(|d| d.join("duplicates.json"))
}

/// DB の状態を確かめ、キャッシュが使えればそれを、使えなければ作り直したものを送る。
fn find(sender: std::sync::mpsc::Sender<Found>) {
    let Some(path) = crate::scan::db_path() else {
        let _ = sender.send(Found::Done(Err("DB の場所が分かりません".into())));
        return;
    };
    let db = match lisa_core::db::Db::open(&path) {
        Ok(db) => db,
        Err(error) => {
            let _ = sender.send(Found::Done(Err(error.to_string())));
            return;
        }
    };
    let signature = lisa_core::dups::signature(&db).ok().flatten();
    let cache = cache_path();
    let cached: Option<lisa_core::dups::Cached> = cache
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if let Some(cached) = cached {
        if signature.as_ref() == Some(&cached.signature) {
            let _ = sender.send(Found::Done(Ok(cached.groups)));
            return;
        }
        let _ = sender.send(Found::Previous(cached.groups));
    }
    let result = lisa_core::dups::groups(&db, MAX_GROUPS).map_err(|e| e.to_string());
    if let (Ok(groups), Some(signature), Some(cache)) = (&result, signature, cache) {
        let cached = lisa_core::dups::Cached {
            signature,
            groups: groups.clone(),
        };
        if let Ok(bytes) = serde_json::to_vec(&cached) {
            if let Some(parent) = cache.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = cache.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &cache);
            }
        }
    }
    let _ = sender.send(Found::Done(result));
}

/// 選んだ組の印。
#[derive(Resource, Default, PartialEq)]
pub struct DupHighlight {
    /// コピーそのもの。
    pub copies: HashSet<NodeId>,
    /// コピーを中に含むフォルダ（コピーがまだ木に出ていないときは、出ている一番深いもの）。
    pub around: HashSet<NodeId>,
}

/// 一覧を開いたら、裏のスレッドで確かめる（キャッシュか、作り直し）。届いたら受け取る。
fn compute(
    mut dups: ResMut<Dups>,
    mut was_open: Local<bool>,
    occluded: Res<crate::view::WindowOccludedState>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let opened = dups.open && !*was_open;
    *was_open = dups.open;
    if opened && !matches!(dups.state, DupsState::Computing { .. }) {
        let previous = match std::mem::take(&mut dups.state) {
            DupsState::Ready(groups) => Some(groups),
            _ => None,
        };
        let (sender, receiver) = channel();
        std::thread::Builder::new()
            .name("lisa-dups".into())
            .spawn(move || find(sender))
            .expect("重複の一覧を作る係を作る");
        dups.state = DupsState::Computing {
            previous,
            receiver: Mutex::new(receiver),
        };
        return;
    }
    let DupsState::Computing { receiver, .. } = &dups.state else {
        return;
    };
    // 止まっている間も、できたかを見に来られるようにする（窓が隠れている間は 1 秒周期に任せる）。
    if !occluded.0 {
        redraw.write(RequestRedraw);
    }
    let found = receiver.lock().ok().and_then(|r| r.try_recv().ok());
    match found {
        Some(Found::Previous(groups)) => {
            if let DupsState::Computing { previous, .. } = &mut dups.state {
                *previous = Some(groups);
            }
        }
        Some(Found::Done(Ok(groups))) => {
            // 前の一覧で選んでいた組は、作り直すと番号が変わるので外す。
            let same = dups.groups().len() == groups.len()
                && dups
                    .groups()
                    .iter()
                    .zip(&groups)
                    .all(|(a, b)| a.hash == b.hash);
            if !same {
                dups.selected = None;
            }
            dups.state = DupsState::Ready(groups);
        }
        Some(Found::Done(Err(error))) => dups.state = DupsState::Failed(error),
        None => {}
    }
}

/// 選んだ組（または整理の計画の対象）と木から、印を付けるノードを決める。変わったら、本物の場面を作り直す。
fn update_highlight(
    dups: Res<Dups>,
    plan_hl: Option<Res<crate::organize::PlanHighlightChains>>,
    tree: Res<FileTree>,
    mut highlight: ResMut<DupHighlight>,
    mut sync: ResMut<SyncRequest>,
) {
    let plan_changed = plan_hl.as_ref().is_some_and(|p| p.is_changed());
    if !dups.is_changed() && !tree.is_changed() && !plan_changed {
        return;
    }
    let mut chains: Vec<&[i64]> = Vec::new();
    if let (true, Some(group)) = (dups.open, dups.selected.and_then(|i| dups.groups().get(i))) {
        chains.extend(group.copies.iter().map(|c| c.chain.as_slice()));
    }
    if let Some(plan_hl) = &plan_hl {
        chains.extend(plan_hl.0.iter().map(Vec::as_slice));
    }

    let mut next = DupHighlight::default();
    if !chains.is_empty() {
        for chain in chains {
            // 木に出ている一番深いところまでたどる。
            let found: Vec<NodeId> = chain.iter().map_while(|&e| tree.node_by_entry(e)).collect();
            let Some(&deepest) = found.last() else {
                continue;
            };
            if found.len() == chain.len() {
                next.copies.insert(deepest);
            } else {
                next.around.insert(deepest);
            }
            next.around.extend(&found[..found.len() - 1]);
        }
        // 木の上の祖先（ドライブより上）にも細い枠。
        let parents: Vec<NodeId> = next
            .copies
            .iter()
            .chain(next.around.iter())
            .flat_map(|&id| {
                let mut list = Vec::new();
                let mut cur = tree.nodes[id].parent;
                while let Some(p) = cur {
                    list.push(p);
                    cur = tree.nodes[p].parent;
                }
                list
            })
            .collect();
        next.around.extend(parents);
        let copies = next.copies.clone();
        next.around.retain(|id| !copies.contains(id));
    }
    if *highlight != next {
        *highlight = next;
        // 印は本物の場面にだけ描くので、操作が止まったタイミングで場面を作り直す。
        sync.request_redraw();
    }
}
