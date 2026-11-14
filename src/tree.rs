//! ファイルの木。Root が端末の一覧を持ち、端末の下にフォルダとファイルが続く。

use bevy::prelude::*;

pub type NodeId = usize;

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
}

/// ダミーの木を書くための入れ物。
enum Entry {
    Dir(&'static str, Vec<Entry>),
    File(&'static str, u64),
}

fn dir(name: &'static str, children: Vec<Entry>) -> Entry {
    Entry::Dir(name, children)
}

fn file(name: &'static str, size: u64) -> Entry {
    Entry::File(name, size)
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
        tree.sort_children();
        tree
    }

    fn insert(&mut self, entry: Entry, parent: NodeId, device: usize) {
        match entry {
            Entry::File(name, size) => {
                self.push(name, size, Some(parent), device, false);
            }
            Entry::Dir(name, children) => {
                let id = self.push(name, 0, Some(parent), device, true);
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
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        self.nodes.push(Node {
            name: name.to_string(),
            size,
            depth,
            parent,
            children: Vec::new(),
            device,
            is_dir,
        });
        if let Some(p) = parent {
            self.nodes[p].children.push(id);
        }
        id
    }

    /// 子を「フォルダを名前順、そのあとファイルを名前順」に並べる。
    fn sort_children(&mut self) {
        for id in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[id].children);
            children.sort_by(|a, b| {
                let (a, b) = (&self.nodes[*a], &self.nodes[*b]);
                b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
            });
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

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
