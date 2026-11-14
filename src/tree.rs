//! ファイルの木。Root が端末の一覧を持ち、端末の下にフォルダとファイルが続く。

use bevy::prelude::*;

pub type NodeId = usize;

/// ダミーの木の定義：(名前, 大きさ)。
type Files = &'static [(&'static str, u64)];
type Folders = &'static [(&'static str, Files)];

pub struct Node {
    pub name: String,
    /// バイト数。フォルダは子の合計。
    pub size: u64,
    pub depth: usize,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// どの端末の下にあるか（色分けに使う）。Root は 0。
    pub device: usize,
}

#[derive(Resource)]
pub struct FileTree {
    pub nodes: Vec<Node>,
}

impl FileTree {
    pub const ROOT: NodeId = 0;

    /// 実験用のダミーの木（Root → 端末 → フォルダ → ファイル の3階層）。
    pub fn dummy() -> Self {
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;
        let devices: &[(&str, Folders)] = &[
            (
                "MacBook",
                &[
                    (
                        "書類",
                        &[
                            ("企画書.pdf", 3 * MB),
                            ("見積もり.xlsx", MB),
                            ("議事録.md", MB / 4),
                            ("契約書.pdf", 2 * MB),
                        ],
                    ),
                    (
                        "写真",
                        &[
                            ("旅行_京都.mov", 30 * GB),
                            ("旅行_札幌.mov", 22 * GB),
                            ("家族.heic", 8 * GB),
                            ("スクショ.png", GB),
                        ],
                    ),
                    (
                        "ダウンロード",
                        &[
                            ("installer.dmg", 12 * GB),
                            ("dataset.zip", 40 * GB),
                            ("manual.pdf", 40 * MB),
                        ],
                    ),
                ],
            ),
            (
                "Windows PC",
                &[
                    (
                        "ドキュメント",
                        &[("レポート.docx", 5 * MB), ("家計簿.xlsx", 2 * MB)],
                    ),
                    (
                        "ゲーム",
                        &[
                            ("game_a", 60 * GB),
                            ("game_b", 35 * GB),
                            ("セーブデータ", 200 * MB),
                        ],
                    ),
                    (
                        "動画",
                        &[
                            ("運動会.mp4", 8 * GB),
                            ("発表会.mp4", 5 * GB),
                            ("編集中.prproj", 300 * MB),
                        ],
                    ),
                ],
            ),
            (
                "外付けSSD",
                &[
                    (
                        "バックアップ",
                        &[
                            ("MacBook_2026-10.sparsebundle", 60 * GB),
                            ("WindowsPC_2026-09.img", 45 * GB),
                        ],
                    ),
                    (
                        "写真アーカイブ",
                        &[
                            ("2023", 20 * GB),
                            ("2024", 30 * GB),
                            ("2025", 35 * GB),
                            ("未整理", 15 * GB),
                        ],
                    ),
                ],
            ),
        ];

        let mut tree = FileTree { nodes: Vec::new() };
        let root = tree.push("Root", 0, None, 0);
        for (i, (device_name, folders)) in devices.iter().enumerate() {
            let device = tree.push(device_name, 0, Some(root), i);
            for (folder_name, files) in folders.iter() {
                let folder = tree.push(folder_name, 0, Some(device), i);
                for (file_name, size) in files.iter() {
                    tree.push(file_name, *size, Some(folder), i);
                }
            }
        }
        tree.sum_sizes(root);
        tree
    }

    fn push(&mut self, name: &str, size: u64, parent: Option<NodeId>, device: usize) -> NodeId {
        let id = self.nodes.len();
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        self.nodes.push(Node {
            name: name.to_string(),
            size,
            depth,
            parent,
            children: Vec::new(),
            device,
        });
        if let Some(p) = parent {
            self.nodes[p].children.push(id);
        }
        id
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
