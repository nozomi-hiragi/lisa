//! カタログの当てはめ（木に対する `match`、`exclude`、`part` の評価）。

use std::collections::HashSet;

use super::content::{Captures, Check, Content, FileFacts};
use super::pattern::{Pattern, Segment};
use super::{Catalog, Role};

/// `**` が下りるフォルダの深さの上限。
pub(crate) const MAX_ANY_DEPTH: usize = 8;

/// 木の 1 つの項目（フォルダまたはファイル）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: i64,
    pub name_nfc: String,
    pub is_dir: bool,
    pub facts: FileFacts,
}

/// 木への問い合わせ。DB でもメモリ上のテスト木でもよい。
pub trait Tree {
    /// フォルダの子。まだ読めていなければ None（確かめ中）。
    fn children(&self, dir: i64) -> Option<Vec<Node>>;
}

/// ファイルの先頭を読む仕組み。
pub trait Reader {
    /// ファイルの先頭 `content.head_len()` バイトを読んで調べた結果。
    /// 読まない・読めないなら None。（呼ぶ側がクラウドのみを避け、結果を覚える。）
    fn head(&mut self, node: &Node, content: &Content) -> Option<Check>;
}

/// カタログを 1 つのフォルダに当てた状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Match,
    Maybe,
    Pending,
    Miss,
}

/// 当てはめの結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub state: State,
    pub captures: Captures,
    /// 外れたとき、どの `match` で外れたか（0 始まり）、または `exclude` の何番目に当たったか。
    pub failed: Option<Failed>,
}

/// 外れた理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Failed {
    Match(usize),
    Exclude(usize),
}

struct Found {
    nodes: Vec<Node>,
    has_unlisted: bool,
}

/// カタログ `cat` がフォルダ `root` に当たるか調べる。
pub fn evaluate(cat: &Catalog, tree: &dyn Tree, root: i64, reader: &mut dyn Reader) -> Outcome {
    // 1. 先にすべての match を名前・形（と、読まずに決まる size/hash）だけで確かめる。
    let mut all_found = Vec::with_capacity(cat.matches.len());
    let mut any_empty_pending = false;

    for (idx, rule) in cat.matches.iter().enumerate() {
        let mut found = find_candidates(tree, root, &rule.path);
        if let Some(content) = &rule.content {
            found
                .nodes
                .retain(|node| content.check(&node.facts, None) != Check::Miss);
        }
        if found.nodes.is_empty() {
            if !found.has_unlisted {
                return Outcome {
                    state: State::Miss,
                    captures: Captures::new(),
                    failed: Some(Failed::Match(idx)),
                };
            }
            any_empty_pending = true;
        }
        all_found.push(found);
    }

    let mut captures = Captures::new();
    let mut any_pending = any_empty_pending;
    let mut any_maybe = false;

    // 2. すべての match で名前・形の候補が 1 つ以上そろったときだけ、中身（Reader）を読む。
    if !any_empty_pending {
        for (idx, (rule, found)) in cat.matches.iter().zip(all_found.iter()).enumerate() {
            let Some(content) = &rule.content else {
                continue;
            };

            let mut hit_caps: Option<Captures> = None;
            let mut saw_unknown = false;
            for node in &found.nodes {
                match check_content(node, content, reader) {
                    Check::Hit(caps) => {
                        hit_caps = Some(caps);
                        break;
                    }
                    Check::Unknown => saw_unknown = true,
                    Check::Miss => {}
                }
            }

            if let Some(caps) = hit_caps {
                // 同じ名前は先の match を優先する。
                for (k, v) in caps {
                    captures.entry(k).or_insert(v);
                }
            } else if found.has_unlisted {
                any_pending = true;
            } else if saw_unknown {
                any_maybe = true;
            } else {
                return Outcome {
                    state: State::Miss,
                    captures: Captures::new(),
                    failed: Some(Failed::Match(idx)),
                };
            }
        }
    }

    // 3. exclude が当たれば Miss（分からないときは無視して進む）。
    for (idx, rule) in cat.excludes.iter().enumerate() {
        let found = find_candidates(tree, root, &rule.path);
        for node in &found.nodes {
            let hit = match &rule.content {
                None => true,
                Some(content) => matches!(check_content(node, content, reader), Check::Hit(_)),
            };
            if hit {
                return Outcome {
                    state: State::Miss,
                    captures: Captures::new(),
                    failed: Some(Failed::Exclude(idx)),
                };
            }
        }
    }

    let state = if any_pending {
        State::Pending
    } else if any_maybe {
        State::Maybe
    } else {
        State::Match
    };

    Outcome {
        state,
        captures,
        failed: None,
    }
}

/// 当たったフォルダの中で、`part` に当たる項目と役割を返す。
/// 同じ項目に複数の `part` が当たれば先に書いたものを使い、読めていないフォルダは飛ばす。
pub fn parts(cat: &Catalog, tree: &dyn Tree, root: i64) -> Vec<(i64, Role)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in &cat.parts {
        let found = find_candidates(tree, root, &part.path);
        for node in found.nodes {
            if seen.insert(node.id) {
                out.push((node.id, part.role));
            }
        }
    }
    out
}

fn check_content(node: &Node, content: &Content, reader: &mut dyn Reader) -> Check {
    let quick = content.check(&node.facts, None);
    match quick {
        Check::Hit(_) | Check::Miss => quick,
        Check::Unknown => {
            if !content.needs_bytes() || node.facts.cloud_only {
                Check::Unknown
            } else {
                reader.head(node, content).unwrap_or(Check::Unknown)
            }
        }
    }
}

struct WalkCtx<'a> {
    tree: &'a dyn Tree,
    target_is_dir: bool,
    seen_ids: HashSet<i64>,
    nodes: Vec<Node>,
    has_unlisted: bool,
}

fn find_candidates(tree: &dyn Tree, root: i64, pattern: &Pattern) -> Found {
    let mut ctx = WalkCtx {
        tree,
        target_is_dir: pattern.is_dir(),
        seen_ids: HashSet::new(),
        nodes: Vec::new(),
        has_unlisted: false,
    };
    walk(&mut ctx, root, pattern.segments(), 0);
    Found {
        nodes: ctx.nodes,
        has_unlisted: ctx.has_unlisted,
    }
}

fn walk(ctx: &mut WalkCtx<'_>, dir: i64, segments: &[Segment], any_depth: usize) {
    let Some(children) = ctx.tree.children(dir) else {
        ctx.has_unlisted = true;
        return;
    };
    walk_children(ctx, &children, segments, any_depth);
}

fn walk_children(ctx: &mut WalkCtx<'_>, children: &[Node], segments: &[Segment], any_depth: usize) {
    let Some((first, rest)) = segments.split_first() else {
        return;
    };

    match first {
        Segment::Name(matcher) => {
            if rest.is_empty() {
                for child in children {
                    if child.is_dir == ctx.target_is_dir
                        && matcher.is_match(&child.name_nfc)
                        && ctx.seen_ids.insert(child.id)
                    {
                        ctx.nodes.push(child.clone());
                    }
                }
            } else {
                for child in children {
                    if child.is_dir && matcher.is_match(&child.name_nfc) {
                        walk(ctx, child.id, rest, any_depth);
                    }
                }
            }
        }
        Segment::Any => {
            // 0 段降りる（このフォルダ自身で次の段を試す）。
            walk_children(ctx, children, rest, any_depth);
            // 1 段以上降りる（深さの上限 MAX_ANY_DEPTH まで）。
            if any_depth < MAX_ANY_DEPTH {
                for child in children {
                    if child.is_dir {
                        walk(ctx, child.id, segments, any_depth + 1);
                    }
                }
            }
        }
    }
}
