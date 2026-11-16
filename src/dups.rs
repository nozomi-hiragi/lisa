//! 重複の一覧（`lisa_core::dups`）を裏で作り、選んだ組のコピーを木の上で光らせる。
//! 表示だけで、ファイルは何も動かさない。
//!
//! 一覧は、画面の横の一覧（`ui/dups.rs`）を初めて開いたときに、裏のスレッドで DB から作る。
//! 選んだ組のコピーは、木にあれば太い枠（`DupHighlight::copies`）、まだ木に出していなければ、
//! 出してある一番深い祖先に細い枠（`around`）を付ける（`render/scene.rs` の `mark_duplicate`）。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use lisa_core::dups::Group;

use crate::model::tree::{FileTree, NodeId};
use crate::render::cache::BakeFinished;

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
    /// 裏で作っている。
    Computing(Mutex<Receiver<Result<Vec<Group>, String>>>),
    Ready(Vec<Group>),
    Failed(String),
}

impl Dups {
    pub fn groups(&self) -> &[Group] {
        match &self.state {
            DupsState::Ready(groups) => groups,
            _ => &[],
        }
    }
}

/// 選んだ組の印。
#[derive(Resource, Default, PartialEq)]
pub struct DupHighlight {
    /// コピーそのもの。
    pub copies: HashSet<NodeId>,
    /// コピーを中に含むフォルダ（コピーがまだ木に出ていないときは、出ている一番深いもの）。
    pub around: HashSet<NodeId>,
}

/// 一覧を開いたら、裏のスレッドで作る。できたら受け取る。
fn compute(mut dups: ResMut<Dups>, mut redraw: MessageWriter<RequestRedraw>) {
    match &dups.state {
        DupsState::NotYet if dups.open => {
            let Some(path) = crate::scan::db_path() else {
                dups.state = DupsState::Failed("DB の場所が分かりません".into());
                return;
            };
            let (sender, receiver) = channel();
            std::thread::Builder::new()
                .name("lisa-dups".into())
                .spawn(move || {
                    let result = lisa_core::db::Db::open(&path)
                        .and_then(|db| lisa_core::dups::groups(&db, MAX_GROUPS))
                        .map_err(|e| e.to_string());
                    let _ = sender.send(result);
                })
                .expect("重複の一覧を作る係を作る");
            dups.state = DupsState::Computing(Mutex::new(receiver));
        }
        DupsState::Computing(receiver) => {
            // 止まっている間も、できたかを見に来られるようにする。
            redraw.write(RequestRedraw);
            let result = receiver.lock().ok().and_then(|r| r.try_recv().ok());
            if let Some(result) = result {
                dups.state = match result {
                    Ok(groups) => DupsState::Ready(groups),
                    Err(error) => DupsState::Failed(error),
                };
            }
        }
        _ => {}
    }
}

/// 選んだ組と木から、印を付けるノードを決める。変わったら、本物の場面を作り直す。
fn update_highlight(
    mut commands: Commands,
    dups: Res<Dups>,
    tree: Res<FileTree>,
    mut highlight: ResMut<DupHighlight>,
) {
    if !dups.is_changed() && !tree.is_changed() {
        return;
    }
    let mut next = DupHighlight::default();
    if let (true, Some(group)) = (dups.open, dups.selected.and_then(|i| dups.groups().get(i))) {
        let by_entry: HashMap<i64, NodeId> = tree
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| node.entry.map(|e| (e, id)))
            .collect();
        for copy in &group.copies {
            // 木に出ている一番深いところまでたどる。
            let found: Vec<NodeId> = copy
                .chain
                .iter()
                .map_while(|e| by_entry.get(e).copied())
                .collect();
            let Some(&deepest) = found.last() else {
                continue;
            };
            if found.len() == copy.chain.len() {
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
        // 印は本物の場面にだけ描くので、場面を作り直す。
        commands.insert_resource(BakeFinished);
    }
}
