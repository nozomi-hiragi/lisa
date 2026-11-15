//! ファイルの木。Root が端末の一覧を持ち、端末の下にフォルダとファイルが続く。

use bevy::prelude::*;

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
        let counted = states.iter().filter(|s| **s != LoadState::Failed);
        let has = |want: LoadState| states.contains(&want);
        let state =
            if counted.clone().all(|s| *s == LoadState::Unloaded) && has(LoadState::Unloaded) {
                LoadState::Unloaded
            } else if has(LoadState::Loading) || has(LoadState::Unloaded) {
                LoadState::Loading
            } else if has(LoadState::Stale) {
                LoadState::Stale
            } else {
                LoadState::Loaded
            };
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
