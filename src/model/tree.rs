//! ファイルの木。Root が端末の一覧を持ち、端末の下にドライブ、フォルダとファイルが続く。
//!
//! 木は読んだところだけを持つ。フォルダの子は、読めたら `add_listing` で足す（`scan.rs`）。

use std::collections::HashMap;
use std::path::PathBuf;

use bevy::prelude::*;
use lisa_core::listing::{Kind, Listing};

pub type NodeId = usize;

/// 読み込みの状態。ファイルはハッシュとプレビュー、フォルダは中身全部とフォルダの画像キャッシュ。
/// 読み込み済みと言えるのは、ハッシュと画像キャッシュが両方そろったとき。中身が変われば
/// ハッシュと画像キャッシュは同じタイミングで作り直すので、そのあいだは古いになる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadState {
    /// まだ読み込んでいない。
    Unloaded,
    /// 読み込んでいる最中（待ち行列に入っているものも含む）。
    Loading,
    /// 読み込み済みで、今の中身と合っている。
    Loaded,
    /// 読み込んだあとに中身が変わった（読み込み直しが要る）。
    Stale,
    /// 読み込めなかった（権限がない、途中で消えた、ディスクのエラーなど）。
    Failed,
}

pub struct Node {
    /// 再利用されるノード番号とは独立した、木に追加された時点の識別子。
    pub(crate) incarnation: u64,
    /// 表示のために相手から受け取ったドライブのマウント先。
    pub(crate) remote_mount: Option<PathBuf>,
    pub(crate) drive_id: Option<String>,
    pub name: String,
    /// バイト数。フォルダは子の合計。
    pub size: u64,
    pub depth: usize,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// どの端末の下にあるか（色分けに使う）。Root は 0。
    pub device: usize,
    /// フォルダ（Root と端末も含む）なら true。
    pub is_dir: bool,
    pub state: LoadState,
    /// ディスク上の場所。読めるフォルダだけ持つ（ダミーの木、別のボリューム、クラウドにだけある
    /// フォルダは無し）。
    pub path: Option<PathBuf>,
    /// 子を読み終えた（読めなかったときも含む）。読み終えていても、子がまだメモリに無いことがある
    /// （`loaded`）。
    pub listed: bool,
    /// 子がメモリ（この木）にある。読み終えて DB にあるが、まだ出していなければ false。
    /// 見えるところに来たら DB から出す（`scan.rs` の `load_visible`）。
    pub loaded: bool,
    /// DB の `entry.id`（DB から出したフォルダだけ）。
    pub entry: Option<i64>,
    /// LAN の相手のノードなら、どの相手かと相手の DB の `entry.id`。
    pub remote: Option<RemoteRef>,
    /// LAN の相手のノードで、相手から届いた更新日時とハッシュ（ツールチップに出す）。
    pub remote_meta: Option<RemoteMeta>,
}

/// LAN の相手から届いた、1 つのファイルかフォルダの情報（`lisa_core::query::Item` の一部）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteMeta {
    /// `file` / `dir` / `symlink`。
    pub kind: String,
    /// 更新日時（UTC の Unix 時刻、ミリ秒）。
    pub mtime: Option<i64>,
    /// 中身のハッシュ（16 進）。
    pub hash: Option<String>,
    pub cloud_only: bool,
    pub error: Option<String>,
}

/// LAN の他の PC のノードが指す相手と、相手の DB の `entry.id`（端末のノード自身は `None`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRef {
    pub device_id: String,
    pub entry_id: Option<i64>,
}

/// 相手の端末のドライブ 1 つ分の情報（木に足すとき用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDrive {
    pub id: Option<String>,
    pub mount: Option<PathBuf>,
    pub label: String,
    pub entry_id: i64,
    pub listed: bool,
    pub has_hash: bool,
    pub has_error: bool,
}

/// ダミーの木を書くための入れ物。
enum Entry {
    Dir(String, Vec<Entry>),
    File(String, u64),
}

fn dir(name: impl Into<String>, children: Vec<Entry>) -> Entry {
    Entry::Dir(name.into(), children)
}

fn file(name: impl Into<String>, size: u64) -> Entry {
    Entry::File(name.into(), size)
}

#[derive(Resource)]
pub struct FileTree {
    pub nodes: Vec<Node>,
    /// `reset_children` で外した空きスロット。次にノードを作るときに再利用して `nodes` の肥大化を防ぐ。
    free: Vec<NodeId>,
    /// 木につながっている手元ノードの DB `entry.id` → `NodeId` 索引（`dups`・`catalog`・`resolve_location` 用）。
    by_entry: HashMap<i64, NodeId>,
    pub(crate) preview_generations: HashMap<PathBuf, u64>,
    pub(crate) preview_revision: u64,
}

impl FileTree {
    pub const ROOT: NodeId = 0;

    fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            free: Vec::new(),
            by_entry: HashMap::new(),
            preview_generations: HashMap::new(),
            preview_revision: 0,
        }
    }

    /// DB の `entry.id` から、現在木につながっているノードを O(1) で引く。
    pub fn node_by_entry(&self, entry: i64) -> Option<NodeId> {
        let &id = self.by_entry.get(&entry)?;
        self.is_attached(id).then_some(id)
    }

    /// 実験用のダミーの木。
    pub fn dummy() -> Self {
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;
        let devices = vec![
            dir(
                "MacBook",
                vec![
                    dir(
                        "書類",
                        vec![
                            file("企画書.pdf", 3 * MB),
                            file("見積もり.xlsx", MB),
                            file("議事録.md", MB / 4),
                            file("契約書.pdf", 2 * MB),
                        ],
                    ),
                    dir(
                        "写真",
                        vec![
                            dir(
                                "旅行",
                                vec![file("京都.mov", 30 * GB), file("札幌.mov", 22 * GB)],
                            ),
                            file("家族.heic", 8 * GB),
                            file("スクショ.png", GB),
                        ],
                    ),
                    dir(
                        "ダウンロード",
                        vec![
                            file("installer.dmg", 12 * GB),
                            file("dataset.zip", 40 * GB),
                            file("manual.pdf", 40 * MB),
                        ],
                    ),
                ],
            ),
            dir(
                "Windows PC",
                vec![
                    dir(
                        "ドキュメント",
                        vec![file("レポート.docx", 5 * MB), file("家計簿.xlsx", 2 * MB)],
                    ),
                    dir(
                        "ゲーム",
                        vec![
                            dir("game_a", vec![file("game_a.exe", 60 * GB)]),
                            dir("game_b", vec![file("game_b.exe", 35 * GB)]),
                            file("セーブデータ.zip", 200 * MB),
                        ],
                    ),
                    dir(
                        "動画",
                        vec![
                            file("運動会.mp4", 8 * GB),
                            file("発表会.mp4", 5 * GB),
                            file("編集中.prproj", 300 * MB),
                        ],
                    ),
                ],
            ),
            dir(
                "外付けSSD",
                vec![
                    dir(
                        "バックアップ",
                        vec![
                            file("MacBook_2026-10.sparsebundle", 60 * GB),
                            file("WindowsPC_2026-09.img", 45 * GB),
                        ],
                    ),
                    dir(
                        "写真アーカイブ",
                        vec![
                            // ファイルが多いフォルダ（タイルが小さくなるときの確認用）。
                            dir(
                                "スキャン",
                                (1..=500)
                                    .map(|i| file(format!("scan_{i:04}.jpg"), (2 + i % 7) * MB))
                                    .collect(),
                            ),
                            dir(
                                "2024",
                                vec![file("春.zip", 12 * GB), file("夏.zip", 18 * GB)],
                            ),
                            dir(
                                "2025",
                                vec![file("春.zip", 15 * GB), file("秋.zip", 20 * GB)],
                            ),
                            file("未整理.zip", 15 * GB),
                        ],
                    ),
                ],
            ),
        ];

        let mut tree = FileTree::empty();
        let root = tree.push("Root", 0, None, 0, true);
        for (i, device) in devices.into_iter().enumerate() {
            tree.insert(device, root, i);
        }
        tree.sum_sizes(root);
        tree.derive_states(root);
        tree.sort_children();
        tree
    }

    /// この端末と、つながっているドライブだけの木。ドライブの中は、開いたら読む。
    pub fn this_device(name: &str, drives: &[lisa_core::volumes::Volume]) -> Self {
        let mut tree = FileTree::empty();
        let root = tree.push("Root", 0, None, 0, true);
        let device = tree.push(name, 0, Some(root), 0, true);
        for drive in drives.iter().filter(|d| !d.hidden) {
            let label = lisa_core::display_path::drive_label(
                &drive.label,
                Some(&drive.mount_path),
                drive.id.as_deref().unwrap_or(""),
            );
            let id = tree.push(&label, 0, Some(device), 0, true);
            tree.nodes[id].drive_id = drive.id.clone();
            tree.nodes[id].path = Some(drive.mount_path.clone());
            tree.nodes[id].listed = false;
            tree.nodes[id].loaded = false;
            tree.nodes[id].state = LoadState::Unloaded;
        }
        tree.nodes[device].state = LoadState::Unloaded;
        tree.nodes[root].state = LoadState::Unloaded;
        tree
    }

    /// ドライブのノード `id` に、DB に残っていた一番上の行を合わせる（子はまだ出さない）。
    pub fn set_drive_row(&mut self, id: NodeId, row: &lisa_core::db::Stored) {
        self.set_entry(id, row.id);
        let node = &mut self.nodes[id];
        node.listed = row.listed;
        if row.error.is_some() {
            node.state = LoadState::Failed;
            node.loaded = true;
        } else if row.hash.is_some() {
            node.state = LoadState::Loaded;
        }
        self.update_ancestor_states(id);
    }

    /// ノード `id` に DB の `entry.id` を紐づけ、索引にも登録する。
    pub fn set_entry(&mut self, id: NodeId, entry: i64) {
        if let Some(old) = self.nodes[id].entry
            && self.by_entry.get(&old) == Some(&id)
        {
            self.by_entry.remove(&old);
        }
        self.nodes[id].entry = Some(entry);
        self.by_entry.insert(entry, id);
    }

    /// 読み終えた（DB にはある）が、子をまだ出していない。ハッシュができるまでは未読み込みのまま。
    pub fn mark_listed(&mut self, id: NodeId) {
        self.nodes[id].listed = true;
    }

    /// フォルダ `id` の子を外す（読み直したら変わっていたので、出し直す）。外した子孫のスロットは
    /// `free` へ戻して再利用し、`nodes` の際限ない肥大化を防ぐ。
    pub fn reset_children(&mut self, id: NodeId) {
        let children = std::mem::take(&mut self.nodes[id].children);
        let removed: u64 = children.iter().map(|&c| self.nodes[c].size).sum();
        let mut stack = children;
        while let Some(child) = stack.pop() {
            let grand = std::mem::take(&mut self.nodes[child].children);
            stack.extend(grand);
            if let Some(entry) = self.nodes[child].entry.take()
                && self.by_entry.get(&entry) == Some(&child)
            {
                self.by_entry.remove(&entry);
            }
            let node = &mut self.nodes[child];
            node.parent = None;
            node.name.clear();
            node.path = None;
            node.remote = None;
            node.remote_meta = None;
            node.remote_mount = None;
            node.drive_id = None;
            node.listed = false;
            node.loaded = false;
            node.size = 0;
            self.free.push(child);
        }
        self.nodes[id].loaded = false;
        let mut cur = Some(id);
        while let Some(i) = cur {
            self.nodes[i].size = self.nodes[i].size.saturating_sub(removed);
            cur = self.nodes[i].parent;
        }
    }

    /// ノード `id` が Root までつながっているか（`reset_children` で親から外されたノードは false）。
    pub fn is_attached(&self, mut id: NodeId) -> bool {
        if id >= self.nodes.len() {
            return false;
        }
        loop {
            if id == Self::ROOT {
                return true;
            }
            match self.nodes[id].parent {
                Some(parent) => id = parent,
                None => return false,
            }
        }
    }

    /// Root から `id` までの道のり。木から外れているノードなら `None`。
    pub fn path_from_root(&self, id: NodeId) -> Option<Vec<NodeId>> {
        if id >= self.nodes.len() {
            return None;
        }
        let mut path = vec![id];
        let mut cur = id;
        while cur != Self::ROOT {
            let parent = self.nodes[cur].parent?;
            path.push(parent);
            cur = parent;
        }
        path.reverse();
        Some(path)
    }

    /// フォルダ `id` は読み直したら変わっていた（ハッシュし直すまで古い）。中のハッシュの無いファイルも
    /// 古いにする。
    pub fn mark_stale(&mut self, id: NodeId) {
        if self.nodes[id].state == LoadState::Failed {
            return;
        }
        let children = self.nodes[id].children.clone();
        for child in children {
            let node = &mut self.nodes[child];
            if !node.is_dir && node.state == LoadState::Unloaded {
                node.state = LoadState::Stale;
            }
        }
        self.nodes[id].state = LoadState::Stale;
        self.update_ancestor_states(id);
    }

    /// フォルダ `id` のハッシュができた（中のファイルもハッシュした）。
    pub fn mark_hashed(&mut self, id: NodeId) {
        if self.nodes[id].state == LoadState::Failed {
            return;
        }
        let children = self.nodes[id].children.clone();
        for child in children {
            let node = &mut self.nodes[child];
            if !node.is_dir && matches!(node.state, LoadState::Unloaded | LoadState::Stale) {
                node.state = LoadState::Loaded;
            }
        }
        self.nodes[id].state = LoadState::Loaded;
        self.update_ancestor_states(id);
    }

    /// DB にあるフォルダ `id` の子（`rows`、子の子は入れない）を足す。`scanned` と `hashed` は、
    /// DB にはまだ確定していないが、読み終えた／ハッシュができたフォルダか（読む係が確定する前に
    /// 知らせが届いていることがある）。
    pub fn add_stored_children(
        &mut self,
        id: NodeId,
        rows: Vec<lisa_core::db::Stored>,
        scanned: impl Fn(&std::path::Path) -> bool,
        hashed: impl Fn(&std::path::Path) -> bool,
    ) {
        if self.nodes[id].loaded {
            return;
        }
        self.nodes[id].listed = true;
        self.nodes[id].loaded = true;
        let device = self.nodes[id].device;
        let base = self.nodes[id].path.clone().unwrap_or_default();
        let parent_hashed = hashed(&base);
        let mut added = 0;
        for row in rows {
            let is_dir = row.kind == Kind::Dir;
            let readable = is_dir && !row.cloud_only && !row.other_volume;
            let child = self.push(&row.name_nfc, row.size, Some(id), device, is_dir);
            self.set_entry(child, row.id);
            // ファイルはハッシュができたら読み込み済み。ハッシュしないもの（クラウドのみ、リンク、
            // 中に入らないフォルダ）は、一覧に載った時点で読み込み済み。
            let skip = row.kind != Kind::File || row.cloud_only;
            self.nodes[child].state = if skip || row.hash.is_some() || parent_hashed {
                LoadState::Loaded
            } else if row.error.is_some() {
                LoadState::Failed
            } else {
                LoadState::Unloaded
            };
            if readable {
                let path = base.join(lisa_core::name::from_raw_bytes(&row.name_raw));
                let node = &mut self.nodes[child];
                node.listed = row.listed || scanned(&path);
                node.loaded = false;
                // ハッシュができたフォルダは、子を出していなくても読み込み済み。
                let done = row.hash.is_some() || hashed(&path);
                node.state = if row.error.is_some() {
                    node.loaded = true;
                    LoadState::Failed
                } else if done {
                    LoadState::Loaded
                } else {
                    LoadState::Unloaded
                };
                node.path = Some(path);
            }
            added += row.size;
        }
        self.finish_children(id, added);
    }

    /// フォルダ `id` を読んだ結果を足す。読めなければ失敗にする。祖先の大きさと状態も直す。
    pub fn add_listing(&mut self, id: NodeId, result: Result<Listing, String>) {
        if self.nodes[id].loaded {
            return;
        }
        self.nodes[id].listed = true;
        self.nodes[id].loaded = true;
        let device = self.nodes[id].device;
        let base = self.nodes[id].path.clone().unwrap_or_default();
        match result {
            Err(_) => self.nodes[id].state = LoadState::Failed,
            Ok(listing) => {
                let mut added = 0;
                for entry in listing.entries {
                    let readable =
                        entry.kind == Kind::Dir && !entry.cloud_only && !entry.other_volume;
                    let is_dir = entry.kind == Kind::Dir;
                    let size = if is_dir { 0 } else { entry.size };
                    let child = self.push(&entry.name_nfc, size, Some(id), device, is_dir);
                    // ファイルはハッシュができるまで未読み込み。ハッシュしないもの（クラウドのみ、リンク、
                    // 中に入らないフォルダ）は読み込み済み。中に入って読むフォルダは、中を読むまで未読み込み。
                    self.nodes[child].state = if entry.kind == Kind::File && !entry.cloud_only {
                        LoadState::Unloaded
                    } else {
                        LoadState::Loaded
                    };
                    if readable {
                        self.nodes[child].state = LoadState::Unloaded;
                        self.nodes[child].path = Some(base.join(&entry.name));
                        self.nodes[child].listed = false;
                        self.nodes[child].loaded = false;
                    }
                    added += size;
                }
                self.finish_children(id, added);
                return;
            }
        }
        self.update_ancestor_states(id);
    }

    /// Root の下に LAN の他の PC の端末ノードを足す（すでにあれば名前と状態を直す）。
    pub fn ensure_remote_device(
        &mut self,
        device_id: &str,
        name: &str,
        initial_state: LoadState,
    ) -> NodeId {
        if let Some(&id) = self.nodes[Self::ROOT].children.iter().find(|&&c| {
            self.nodes[c]
                .remote
                .as_ref()
                .is_some_and(|r| r.device_id == device_id)
        }) {
            self.nodes[id].name = name.to_string();
            return id;
        }
        let next_device = self.nodes[Self::ROOT]
            .children
            .iter()
            .map(|&c| self.nodes[c].device)
            .max()
            .map_or(1, |m| m + 1);
        let id = self.push(name, 0, Some(Self::ROOT), next_device, true);
        let node = &mut self.nodes[id];
        node.state = initial_state;
        node.listed = true;
        node.loaded = false;
        node.remote = Some(RemoteRef {
            device_id: device_id.to_string(),
            entry_id: None,
        });
        self.update_ancestor_states(id);
        id
    }

    /// 相手のノード `id`（端末またはフォルダ）の読み込み状態を変える（読み込み中・失敗・未読み込みなど）。
    pub fn set_remote_state(&mut self, id: NodeId, state: LoadState, loaded: bool) {
        self.nodes[id].state = state;
        self.nodes[id].loaded = loaded;
        self.update_ancestor_states(id);
    }

    /// 相手の端末ノード `id` に、`lisa_drives` で取ったドライブ一覧を子として足す。
    pub fn add_remote_drives(&mut self, id: NodeId, drives: Vec<RemoteDrive>) {
        if self.nodes[id].loaded {
            return;
        }
        let Some(device_id) = self.nodes[id].remote.as_ref().map(|r| r.device_id.clone()) else {
            return;
        };
        let device = self.nodes[id].device;
        self.nodes[id].listed = true;
        self.nodes[id].loaded = true;
        for drive in drives {
            let child = self.push(&drive.label, 0, Some(id), device, true);
            let node = &mut self.nodes[child];
            node.drive_id = drive.id;
            node.remote_mount = drive.mount;
            node.listed = drive.listed;
            node.loaded = false;
            node.remote = Some(RemoteRef {
                device_id: device_id.clone(),
                entry_id: Some(drive.entry_id),
            });
            node.state = if drive.has_error {
                node.loaded = true;
                LoadState::Failed
            } else if drive.has_hash {
                LoadState::Loaded
            } else {
                LoadState::Unloaded
            };
        }
        self.finish_children(id, 0);
    }

    /// 相手のフォルダ（またはドライブ）`id` に、`lisa_list` で取った子（`items`）を足す。
    pub fn add_remote_children(&mut self, id: NodeId, items: Vec<lisa_core::query::Item>) {
        if self.nodes[id].loaded {
            return;
        }
        let Some(device_id) = self.nodes[id].remote.as_ref().map(|r| r.device_id.clone()) else {
            return;
        };
        self.nodes[id].listed = true;
        self.nodes[id].loaded = true;
        let device = self.nodes[id].device;
        let mut added = 0;
        for item in items {
            let is_dir = item.kind == "dir";
            let readable = is_dir && !item.cloud_only && !item.other_volume;
            let size = if is_dir { 0 } else { item.size };
            let child = self.push(&item.name, size, Some(id), device, is_dir);
            let skip = item.kind != "file" || item.cloud_only;
            self.nodes[child].state = if skip || item.hash.is_some() {
                LoadState::Loaded
            } else if item.error.is_some() {
                LoadState::Failed
            } else {
                LoadState::Unloaded
            };
            self.nodes[child].remote = Some(RemoteRef {
                device_id: device_id.clone(),
                entry_id: Some(item.id),
            });
            self.nodes[child].remote_meta = Some(RemoteMeta {
                kind: item.kind.clone(),
                mtime: item.mtime,
                hash: item.hash.clone(),
                cloud_only: item.cloud_only,
                error: item.error.clone(),
            });
            if readable {
                let node = &mut self.nodes[child];
                node.listed = item.listed;
                node.loaded = false;
                node.state = if item.error.is_some() {
                    node.loaded = true;
                    LoadState::Failed
                } else if item.hash.is_some() {
                    LoadState::Loaded
                } else {
                    LoadState::Unloaded
                };
            }
            added += size;
        }
        self.finish_children(id, added);
    }

    /// 子を足し終えたフォルダ `id` の状態と、祖先の大きさ（`added` を足す）と状態を直す。
    fn finish_children(&mut self, id: NodeId, added: u64) {
        // 子が無ければ、読み終えた空のフォルダ。
        self.nodes[id].state = if self.nodes[id].children.is_empty() {
            LoadState::Loaded
        } else {
            self.state_from_children(id)
        };
        let mut cur = Some(id);
        while let Some(i) = cur {
            self.nodes[i].size += added;
            cur = self.nodes[i].parent;
        }
        self.update_ancestor_states(id);
    }

    /// 祖先の状態を子から決め直す。
    fn update_ancestor_states(&mut self, id: NodeId) {
        let mut cur = self.nodes[id].parent;
        while let Some(i) = cur {
            self.nodes[i].state = self.state_from_children(i);
            cur = self.nodes[i].parent;
        }
    }

    /// 子の状態からフォルダの状態を決める（`derive_states` と同じ決め方、1 段だけ）。
    fn state_from_children(&self, id: NodeId) -> LoadState {
        let node = &self.nodes[id];
        if node.state == LoadState::Failed || node.children.is_empty() {
            return node.state;
        }
        let states: Vec<LoadState> = node.children.iter().map(|&c| self.nodes[c].state).collect();
        combine(&states)
    }

    /// テスト用: フォルダ `dirs` 個に、それぞれファイル `files` 個。
    #[cfg(test)]
    pub fn wide(dirs: usize, files: usize) -> Self {
        let mut tree = FileTree::empty();
        let root = tree.push("Root", 0, None, 0, true);
        for i in 0..dirs {
            let children = (0..files).map(|j| file(format!("f{j:04}"), 1)).collect();
            tree.insert(dir(format!("d{i:02}"), children), root, 0);
        }
        tree.sort_children();
        tree
    }

    /// テスト用: `levels` 段の深さの木。各段にフォルダ 1 つとファイル 5 つ。
    #[cfg(test)]
    pub fn deep(levels: usize) -> Self {
        let mut entry = dir("底", vec![file("a", 1)]);
        for i in (0..levels).rev() {
            let mut children: Vec<Entry> = (0..5).map(|j| file(format!("f{j}"), 1)).collect();
            children.push(entry);
            entry = dir(format!("d{i:02}"), children);
        }
        let mut tree = FileTree::empty();
        let root = tree.push("Root", 0, None, 0, true);
        tree.insert(entry, root, 0);
        tree.sum_sizes(root);
        tree.sort_children();
        tree
    }

    fn insert(&mut self, entry: Entry, parent: NodeId, device: usize) {
        match entry {
            Entry::File(name, size) => {
                self.push(&name, size, Some(parent), device, false);
            }
            Entry::Dir(name, children) => {
                let id = self.push(&name, 0, Some(parent), device, true);
                for child in children {
                    self.insert(child, id, device);
                }
            }
        }
    }

    fn push(
        &mut self,
        name: &str,
        size: u64,
        parent: Option<NodeId>,
        device: usize,
        is_dir: bool,
    ) -> NodeId {
        let state = dummy_state(name);
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        let node = Node {
            incarnation: next_incarnation(),
            remote_mount: None,
            drive_id: None,
            name: name.to_string(),
            size,
            depth,
            parent,
            children: Vec::new(),
            device,
            is_dir,
            state,
            path: None,
            listed: true,
            loaded: true,
            entry: None,
            remote: None,
            remote_meta: None,
        };
        let id = match self.free.pop() {
            Some(slot) => {
                self.nodes[slot] = node;
                slot
            }
            None => {
                let id = self.nodes.len();
                self.nodes.push(node);
                id
            }
        };
        if let Some(p) = parent {
            self.nodes[p].children.push(id);
        }
        id
    }

    /// 子を、フォルダとファイルをまとめて名前順に並べる。
    fn sort_children(&mut self) {
        for id in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[id].children);
            children.sort_by(|a, b| self.nodes[*a].name.cmp(&self.nodes[*b].name));
            self.nodes[id].children = children;
        }
    }

    fn sum_sizes(&mut self, id: NodeId) -> u64 {
        let children = self.nodes[id].children.clone();
        if !children.is_empty() {
            self.nodes[id].size = children.into_iter().map(|c| self.sum_sizes(c)).sum();
        }
        self.nodes[id].size
    }

    /// フォルダの状態を子から決める。読み込み中か未読み込みの子があれば、全部が未読み込みなら
    /// 未読み込み、そうでなければ読み込み中。次に古い子があれば古い。子の失敗はフォルダの失敗に
    /// しない（フォルダの失敗は、そのフォルダの一覧が読めなかったときだけ）。
    fn derive_states(&mut self, id: NodeId) -> LoadState {
        let children = self.nodes[id].children.clone();
        if !self.nodes[id].is_dir
            || children.is_empty()
            || self.nodes[id].state == LoadState::Failed
        {
            return self.nodes[id].state;
        }
        let states: Vec<LoadState> = children
            .into_iter()
            .map(|c| self.derive_states(c))
            .collect();
        let state = combine(&states);
        self.nodes[id].state = state;
        state
    }

    /// Root からそのノードまでの名前を " / " でつなぐ。
    pub fn path(&self, id: NodeId) -> String {
        let mut names = Vec::new();
        let mut cur = Some(id);
        while let Some(i) = cur {
            names.push(self.nodes[i].name.as_str());
            cur = self.nodes[i].parent;
        }
        names.reverse();
        names.join(" / ")
    }
}

/// 子の状態からフォルダの状態を決める。読み込み中か未読み込みの子があれば、全部が未読み込みなら
/// 未読み込み、そうでなければ読み込み中。次に古い子があれば古い。子の失敗はフォルダの失敗に
/// しない（フォルダの失敗は、そのフォルダの一覧が読めなかったときだけ）。
fn next_incarnation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn combine(states: &[LoadState]) -> LoadState {
    let mut counted = states.iter().filter(|s| **s != LoadState::Failed);
    let has = |want: LoadState| states.contains(&want);
    if counted.all(|s| *s == LoadState::Unloaded) && has(LoadState::Unloaded) {
        LoadState::Unloaded
    } else if has(LoadState::Loading) || has(LoadState::Unloaded) {
        LoadState::Loading
    } else if has(LoadState::Stale) {
        LoadState::Stale
    } else {
        LoadState::Loaded
    }
}

/// ダミーの木の読み込みの状態。見た目を確かめられるよう、名前でいくつかに散らす。
fn dummy_state(name: &str) -> LoadState {
    if let Some(n) = name
        .strip_prefix("scan_")
        .and_then(|rest| rest.strip_suffix(".jpg"))
        .and_then(|n| n.parse::<u32>().ok())
    {
        // 前から順に読み込んでいる途中、というつもり。ところどころ失敗している。
        return match n {
            _ if n % 97 == 0 => LoadState::Failed,
            1..=300 => LoadState::Loaded,
            301..=312 => LoadState::Loading,
            _ => LoadState::Unloaded,
        };
    }
    match name {
        "dataset.zip" | "manual.pdf" => LoadState::Loading,
        "MacBook_2026-10.sparsebundle" | "WindowsPC_2026-09.img" | "未整理.zip" => {
            LoadState::Unloaded
        }
        "秋.zip" | "編集中.prproj" | "見積もり.xlsx" => LoadState::Stale,
        // game_a はフォルダの一覧そのものが読めなかった、というつもり。
        "installer.dmg" | "セーブデータ.zip" | "家族.heic" | "game_a" => LoadState::Failed,
        _ => LoadState::Loaded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("lisa-tree-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn one_drive(path: &std::path::Path) -> FileTree {
        FileTree::this_device("端末", &[one_drive_volume(path)])
    }

    fn one_drive_volume(path: &std::path::Path) -> lisa_core::volumes::Volume {
        lisa_core::volumes::Volume {
            id: None,
            label: "テスト".into(),
            fs: String::new(),
            mount_path: path.to_path_buf(),
            capacity: None,
            available: None,
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: None,
            hidden: false,
        }
    }

    #[test]
    fn listing_adds_children_and_sizes() {
        let dir = temp_dir("add");
        std::fs::write(dir.join("a.txt"), b"12345").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let mut tree = one_drive(&dir);
        let drive = tree.nodes[tree.nodes[FileTree::ROOT].children[0]].children[0];
        assert!(!tree.nodes[drive].listed);

        tree.add_listing(
            drive,
            lisa_core::listing::read_dir(&dir).map_err(|e| e.to_string()),
        );
        let node = &tree.nodes[drive];
        assert!(node.listed);
        assert_eq!(node.children.len(), 2);
        assert_eq!(node.size, 5);
        assert_eq!(tree.nodes[FileTree::ROOT].size, 5);
        let sub = node
            .children
            .iter()
            .copied()
            .find(|&c| tree.nodes[c].name == "sub")
            .unwrap();
        assert!(tree.nodes[sub].is_dir);
        assert!(!tree.nodes[sub].listed);
        assert_eq!(
            tree.nodes[sub].path.as_deref(),
            Some(dir.join("sub").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_tree_comes_back_like_a_listing() {
        let dir = temp_dir("stored");
        std::fs::write(dir.join("a.txt"), b"12345").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.txt"), b"xy").unwrap();
        std::fs::create_dir(dir.join("unread")).unwrap();
        let volume = lisa_core::volumes::Volume {
            id: Some("test:1".into()),
            ..one_drive_volume(&dir)
        };
        let mut db = lisa_core::db::Db::open_in_memory().unwrap();
        let device = db.this_device("端末", "host", "macos").unwrap();
        db.upsert_drive(&device, &volume).unwrap();
        let read = |p: &std::path::Path| lisa_core::listing::read_dir(p).map_err(|e| e.to_string());
        db.save_listing("test:1", &dir, &dir, &read(&dir)).unwrap();
        let sub_path = dir.join("sub");
        db.save_listing("test:1", &dir, &sub_path, &read(&sub_path))
            .unwrap();

        let mut tree = FileTree::this_device("端末", &[volume]);
        let drive = tree.nodes[tree.nodes[FileTree::ROOT].children[0]].children[0];
        let root = db.drive_root("test:1").unwrap().unwrap();
        tree.set_drive_row(drive, &root);
        assert!(tree.nodes[drive].listed && !tree.nodes[drive].loaded);
        tree.add_stored_children(drive, db.children(root.id).unwrap(), |_| false, |_| false);
        let node = &tree.nodes[drive];
        assert!(node.loaded);
        assert_eq!(node.children.len(), 3);
        assert_eq!(node.size, 5);
        let child = |tree: &FileTree, name: &str| {
            tree.nodes[drive]
                .children
                .iter()
                .copied()
                .find(|&c| tree.nodes[c].name == name)
                .unwrap()
        };
        // sub は読み終えているが、子はまだ出していない。出すと大きさが足される。
        let sub = child(&tree, "sub");
        assert!(tree.nodes[sub].listed && !tree.nodes[sub].loaded);
        let sub_entry = tree.nodes[sub].entry.unwrap();
        tree.add_stored_children(sub, db.children(sub_entry).unwrap(), |_| false, |_| false);
        assert_eq!(tree.nodes[sub].size, 2);
        assert_eq!(tree.nodes[drive].size, 7);
        assert!(!tree.nodes[child(&tree, "unread")].listed);
        let child = |name: &str| child(&tree, name);
        assert_eq!(
            tree.nodes[child("unread")].path.as_deref(),
            Some(dir.join("unread").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_children_and_stale_until_hashed() {
        let dir = temp_dir("reset");
        std::fs::write(dir.join("a.txt"), b"12345").unwrap();
        let mut tree = one_drive(&dir);
        let drive = tree.nodes[tree.nodes[FileTree::ROOT].children[0]].children[0];
        let read = || lisa_core::listing::read_dir(&dir).map_err(|e| e.to_string());
        tree.add_listing(drive, read());
        tree.mark_hashed(drive);
        assert_eq!(tree.nodes[drive].state, LoadState::Loaded);
        // 読み直したら変わっていた: 子を外して出し直し、ハッシュし直すまで古い。
        let len_before = tree.nodes.len();
        tree.reset_children(drive);
        assert!(tree.nodes[drive].children.is_empty() && !tree.nodes[drive].loaded);
        assert_eq!(tree.nodes[drive].size, 0);
        // 繰り返し reset_children しても空きスロットを再利用し、nodes が際限なく膨らまない。
        for _ in 0..20 {
            tree.add_listing(drive, read());
            assert_eq!(tree.nodes.len(), len_before);
            tree.reset_children(drive);
        }
        std::fs::write(dir.join("b.txt"), b"xy").unwrap();
        tree.add_listing(drive, read());
        tree.mark_stale(drive);
        assert_eq!(tree.nodes[drive].children.len(), 2);
        assert_eq!(tree.nodes[drive].size, 7);
        assert_eq!(tree.nodes[drive].state, LoadState::Stale);
        assert_eq!(tree.nodes[FileTree::ROOT].state, LoadState::Stale);
        tree.mark_hashed(drive);
        assert_eq!(tree.nodes[drive].state, LoadState::Loaded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_folder_fails() {
        let dir = temp_dir("fail");
        let mut tree = one_drive(&dir.join("nope"));
        let drive = tree.nodes[tree.nodes[FileTree::ROOT].children[0]].children[0];
        tree.add_listing(drive, Err("Operation not permitted".into()));
        assert_eq!(tree.nodes[drive].state, LoadState::Failed);
        assert!(tree.nodes[drive].listed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_folder_is_loaded() {
        let dir = temp_dir("empty");
        let mut tree = one_drive(&dir);
        let drive = tree.nodes[tree.nodes[FileTree::ROOT].children[0]].children[0];
        tree.add_listing(
            drive,
            lisa_core::listing::read_dir(&dir).map_err(|e| e.to_string()),
        );
        assert_eq!(tree.nodes[drive].state, LoadState::Loaded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_drives_and_children_populate_tree() {
        let dir = temp_dir("remote");
        let mut tree = one_drive(&dir);
        let remote_dev = tree.ensure_remote_device("peer-1", "Windows PC", LoadState::Unloaded);
        assert_eq!(tree.nodes[remote_dev].device, 1);
        assert!(tree.nodes[remote_dev].path.is_none());
        assert_eq!(
            tree.nodes[remote_dev].remote,
            Some(RemoteRef {
                device_id: "peer-1".into(),
                entry_id: None
            })
        );

        tree.add_remote_drives(
            remote_dev,
            vec![RemoteDrive {
                id: None,
                mount: None,
                label: "C:".into(),
                entry_id: 10,
                listed: true,
                has_hash: false,
                has_error: false,
            }],
        );
        assert!(tree.nodes[remote_dev].loaded);
        let drive = tree.nodes[remote_dev].children[0];
        assert_eq!(tree.nodes[drive].name, "C:");
        assert_eq!(tree.nodes[drive].device, 1);
        assert!(tree.nodes[drive].path.is_none());
        assert!(tree.nodes[drive].listed && !tree.nodes[drive].loaded);
        assert_eq!(
            tree.nodes[drive].remote,
            Some(RemoteRef {
                device_id: "peer-1".into(),
                entry_id: Some(10)
            })
        );

        let items = vec![
            lisa_core::query::Item {
                id: 11,
                name: "docs".into(),
                kind: "dir".into(),
                size: 0,
                mtime: None,
                hash: Some("a".repeat(64)),
                cloud_only: false,
                other_volume: false,
                listed: true,
                error: None,
            },
            lisa_core::query::Item {
                id: 12,
                name: "hashed.txt".into(),
                kind: "file".into(),
                size: 100,
                mtime: None,
                hash: Some("b".repeat(64)),
                cloud_only: false,
                other_volume: false,
                listed: false,
                error: None,
            },
            lisa_core::query::Item {
                id: 13,
                name: "unhashed.txt".into(),
                kind: "file".into(),
                size: 50,
                mtime: None,
                hash: None,
                cloud_only: false,
                other_volume: false,
                listed: false,
                error: None,
            },
            lisa_core::query::Item {
                id: 14,
                name: "broken.txt".into(),
                kind: "file".into(),
                size: 20,
                mtime: None,
                hash: None,
                cloud_only: false,
                other_volume: false,
                listed: false,
                error: Some("permission denied".into()),
            },
        ];
        tree.add_remote_children(drive, items);
        assert!(tree.nodes[drive].loaded);
        assert_eq!(tree.nodes[drive].size, 170);
        assert_eq!(tree.nodes[remote_dev].size, 170);

        let find = |name: &str| {
            tree.nodes[drive]
                .children
                .iter()
                .copied()
                .find(|&c| tree.nodes[c].name == name)
                .unwrap()
        };
        let docs = find("docs");
        assert!(tree.nodes[docs].is_dir);
        assert!(tree.nodes[docs].listed && !tree.nodes[docs].loaded);
        assert_eq!(tree.nodes[docs].state, LoadState::Loaded);
        assert!(tree.nodes[docs].path.is_none());
        assert_eq!(
            tree.nodes[docs].remote,
            Some(RemoteRef {
                device_id: "peer-1".into(),
                entry_id: Some(11)
            })
        );
        assert_eq!(tree.nodes[find("hashed.txt")].state, LoadState::Loaded);
        assert_eq!(tree.nodes[find("unhashed.txt")].state, LoadState::Unloaded);
        assert_eq!(tree.nodes[find("broken.txt")].state, LoadState::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
