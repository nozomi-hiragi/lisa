//! ファイルの木。Root が端末の一覧を持ち、端末の下にドライブ、フォルダとファイルが続く。
//!
//! 木は読んだところだけを持つ。フォルダの子は、読めたら `add_listing` で足す（`scan.rs`）。

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
    /// 子を読み終えた（読めなかったときも含む）。
    pub listed: bool,
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
}

impl FileTree {
    pub const ROOT: NodeId = 0;

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

        let mut tree = FileTree { nodes: Vec::new() };
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
        let mut tree = FileTree { nodes: Vec::new() };
        let root = tree.push("Root", 0, None, 0, true);
        let device = tree.push(name, 0, Some(root), 0, true);
        for drive in drives.iter().filter(|d| !d.hidden) {
            let label = if drive.label.is_empty() {
                drive.mount_path.display().to_string()
            } else {
                drive.label.clone()
            };
            let id = tree.push(&label, 0, Some(device), 0, true);
            tree.nodes[id].path = Some(drive.mount_path.clone());
            tree.nodes[id].listed = false;
            tree.nodes[id].state = LoadState::Unloaded;
        }
        tree.nodes[device].state = LoadState::Unloaded;
        tree.nodes[root].state = LoadState::Unloaded;
        tree
    }

    /// フォルダ `id` を読んだ結果を足す。読めなければ失敗にする。祖先の大きさと状態も直す。
    pub fn add_listing(&mut self, id: NodeId, result: Result<Listing, String>) {
        if self.nodes[id].listed {
            return;
        }
        self.nodes[id].listed = true;
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
                    // まだハッシュしていないので、ファイルは未読み込み。フォルダは中を読むまで未読み込み。
                    self.nodes[child].state = LoadState::Unloaded;
                    if readable {
                        self.nodes[child].path = Some(base.join(&entry.name));
                        self.nodes[child].listed = false;
                    }
                    added += size;
                }
                // 子が無ければ、読み終えた空のフォルダ。
                self.nodes[id].state = if self.nodes[id].children.is_empty() {
                    LoadState::Loaded
                } else {
                    LoadState::Unloaded
                };
                let mut cur = Some(id);
                while let Some(i) = cur {
                    self.nodes[i].size += added;
                    cur = self.nodes[i].parent;
                }
            }
        }
        // 祖先の状態を子から決め直す。
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
        let mut tree = FileTree { nodes: Vec::new() };
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
        let mut tree = FileTree { nodes: Vec::new() };
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
        let id = self.nodes.len();
        let state = dummy_state(name);
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        self.nodes.push(Node {
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
        });
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
        let drive = lisa_core::volumes::Volume {
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
        };
        FileTree::this_device("端末", &[drive])
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
}
