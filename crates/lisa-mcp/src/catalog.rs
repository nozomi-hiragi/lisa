//! カタログ（型）と 1 か所の仕分けを扱う MCP の道具。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use lisa_core::catalog::db::{self as catalog_db, DbTree};
use lisa_core::catalog::{self, Catalog, CatalogSet, CatalogSettings, Loaded, Role, State};
use lisa_core::db::{Db, Stored};
use lisa_core::listing::Kind;
use lisa_core::query;
use serde_json::{Value, json};

use super::{DEFAULT_LIMIT, MAX_LIMIT, Server, arg_usize, err};

/// `lisa_unclassified` の既定の最小サイズ（1 GiB）。
const DEFAULT_MIN_SIZE: u64 = 1024 * 1024 * 1024;
/// `lisa_unclassified` の既定の件数。
const DEFAULT_UNCLASSIFIED_LIMIT: usize = 30;
/// `lisa_catalog_test` の既定の件数。
const DEFAULT_TEST_LIMIT: usize = 50;
/// `lisa_outline` の木の深さの既定と上限。
const DEFAULT_OUTLINE_DEPTH: usize = 3;
const MAX_OUTLINE_DEPTH: usize = 6;
/// `lisa_outline` で 1 つのフォルダに出す子の最大数。
const OUTLINE_CHILDREN_PER_LEVEL: usize = 40;
/// `lisa_outline` で出す拡張子と名前の最大数。
const OUTLINE_TOP_STATS: usize = 20;
/// `lisa_outline` で先頭を読むテキストファイルの最大バイト数の既定と上限。
const DEFAULT_TEXT_BYTES: usize = 400;
const MAX_TEXT_BYTES: usize = 4000;
/// `lisa_outline` で中身を読むテキストファイルの大きさの上限（64 KiB）。
const MAX_TEXT_FILE_SIZE: u64 = 64 * 1024;
/// `lisa_outline` で読むテキストファイルの数。
const MAX_TEXT_SAMPLES: usize = 5;
/// `lisa_outline` で先頭を読むテキストファイルの拡張子（小文字）。
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "json", "toml", "plist", "cfg", "ini", "yaml", "yml", "xml", "vdf", "acf",
];

impl Server {
    /// `settings.json` の場所（テスト用に `data_dir` の中があればそれを優先し、無ければアプリと同じ）。
    fn settings_path(&self) -> Option<PathBuf> {
        let local = self.data_dir.join("settings.json");
        if local.exists() {
            return Some(local);
        }
        if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(dir).join("settings.json"));
        }
        directories::ProjectDirs::from("", "", "Lisa").map(|d| d.config_dir().join("settings.json"))
    }

    /// カタログを読むフォルダの一覧と、組み込み＋ユーザーフォルダから読んだカタログの集まり。
    fn load_catalog_set(&self) -> (Vec<PathBuf>, CatalogSet) {
        let settings = self
            .settings_path()
            .map(|p| CatalogSettings::from_file(&p))
            .unwrap_or_default();
        let dirs = catalog::dirs(&self.data_dir, &settings);
        let set = CatalogSet::load_with_builtin(&dirs);
        (dirs, set)
    }

    fn db_if_exists(&self) -> Result<Option<Db>, String> {
        let path = self.data_dir.join("lisa.db");
        if !path.exists() {
            return Ok(None);
        }
        Db::open(&path)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// 読んでいるカタログ（型）の一覧とエラー。
    pub(super) fn catalogs(&self, args: &Value) -> Result<Value, String> {
        let include_text = args
            .get("include_text")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (dirs, set) = self.load_catalog_set();
        let counts = match self.db_if_exists()? {
            Some(db) => catalog_db::counts(&db).map_err(err)?,
            None => HashMap::new(),
        };

        let catalogs: Vec<Value> = set
            .catalogs
            .iter()
            .map(|loaded| catalog_summary(loaded, &counts, include_text))
            .collect();
        let errors: Vec<Value> = set
            .errors
            .iter()
            .map(|(source, msg)| {
                json!({
                    "source": source.to_string_lossy(),
                    "error": msg,
                })
            })
            .collect();
        let dirs_json: Vec<String> = dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();

        Ok(json!({
            "dirs": dirs_json,
            "catalogs": catalogs,
            "errors": errors,
        }))
    }

    /// 型ごと・アプリごとの当たったフォルダと 1 か所の仕分けの一覧。
    pub(super) fn catalog_matches(&self, args: &Value) -> Result<Value, String> {
        let mut db = self.db()?;
        let (_, set) = self.load_catalog_set();

        let catalog_filter = args.get("catalog").and_then(Value::as_str);
        let app_filter = args.get("app").and_then(Value::as_str);
        let drive_filter = args.get("drive_id").and_then(Value::as_str);
        let limit = arg_usize(args, "limit", DEFAULT_LIMIT).min(MAX_LIMIT);
        let totals = args.get("totals").and_then(Value::as_bool).unwrap_or(true);
        let with_parts = args.get("parts").and_then(Value::as_bool).unwrap_or(false);

        let drive_mounts: HashMap<String, PathBuf> = query::drives(&db)
            .map_err(err)?
            .into_iter()
            .filter_map(|d| d.mount.map(|m| (d.id, m)))
            .collect();

        let mut rows: Vec<MatchCandidate> = Vec::new();

        // 1. 仕分け（assignment）。catalog 絞り込みが無いときだけ含める。
        if catalog_filter.is_none() {
            let resolved = catalog_db::resolve_assignments(&mut db).map_err(err)?;
            for (a, entry_id) in resolved {
                if app_filter.is_some_and(|f| a.app != f) {
                    continue;
                }
                if drive_filter.is_some_and(|f| a.drive_id != f) {
                    continue;
                }
                let path = match entry_id.and_then(|eid| query::path_of(&db, eid).ok().flatten()) {
                    Some(loc) => loc.path.to_string_lossy().into_owned(),
                    None => match drive_mounts.get(&a.drive_id) {
                        Some(mount) if a.path.is_empty() => mount.to_string_lossy().into_owned(),
                        Some(mount) => mount.join(&a.path).to_string_lossy().into_owned(),
                        None => a.path.clone(),
                    },
                };
                let state = if entry_id.is_some() {
                    "assigned"
                } else {
                    "lost"
                };
                rows.push(MatchCandidate {
                    entry_id,
                    catalog_id: None,
                    app: a.app.clone(),
                    state,
                    label: a.app.clone(),
                    path,
                    drive_id: a.drive_id.clone(),
                    assignment: Some(a),
                });
            }
        }

        // 2. カタログの当たり（catalog_match）。
        let match_rows = match catalog_filter {
            Some(id) => catalog_db::matches_of(&db, id).map_err(err)?,
            None => catalog_db::matches(&db).map_err(err)?,
        };
        for row in match_rows {
            let loaded = set.get(&row.catalog_id);
            let row_app = loaded
                .map(|l| l.catalog.app.as_str())
                .unwrap_or("")
                .to_string();
            if app_filter.is_some_and(|f| row_app != f) {
                continue;
            }
            let Some(loc) = query::path_of(&db, row.entry_id).map_err(err)? else {
                continue;
            };
            if drive_filter.is_some_and(|f| loc.drive_id != f) {
                continue;
            }
            let label = loaded
                .map(|l| l.catalog.label_text(&row.captures))
                .unwrap_or_else(|| row.catalog_id.clone());
            rows.push(MatchCandidate {
                entry_id: Some(row.entry_id),
                catalog_id: Some(row.catalog_id),
                app: row_app,
                state: state_str(row.state),
                label,
                path: loc.path.to_string_lossy().into_owned(),
                drive_id: loc.drive_id,
                assignment: None,
            });
        }

        let total = rows.len();
        let mut by_catalog_map: HashMap<String, (String, usize, u64)> = HashMap::new();
        let mut by_app_map: HashMap<String, (usize, u64)> = HashMap::new();
        for r in &rows {
            if let Some(cid) = &r.catalog_id {
                let entry = by_catalog_map
                    .entry(cid.clone())
                    .or_insert_with(|| (r.app.clone(), 0, 0));
                entry.1 += 1;
            }
            let app_entry = by_app_map.entry(r.app.clone()).or_insert((0, 0));
            app_entry.0 += 1;
        }

        // 一覧では上位 limit 件だけ folder_total を計算する（重い走査を避けるため）。
        let mut shown: Vec<(MatchCandidate, Option<u64>)> =
            rows.into_iter().take(limit).map(|r| (r, None)).collect();
        if totals {
            for (r, size_slot) in &mut shown {
                if let Some(eid) = r.entry_id {
                    let (_, _, bytes) = query::folder_total(&db, eid).map_err(err)?;
                    *size_slot = Some(bytes);
                    if let Some(cid) = &r.catalog_id
                        && let Some(entry) = by_catalog_map.get_mut(cid)
                    {
                        entry.2 += bytes;
                    }
                    if let Some(app_entry) = by_app_map.get_mut(&r.app) {
                        app_entry.1 += bytes;
                    }
                }
            }
            shown.sort_by_key(|(_, sz)| std::cmp::Reverse(sz.unwrap_or(0)));
        }

        let tree = with_parts.then(|| DbTree::new(&db));
        let mut matches_json = Vec::with_capacity(shown.len());
        for (r, size_opt) in shown {
            let mut obj = json!({
                "entry_id": r.entry_id,
                "catalog": r.catalog_id,
                "app": r.app,
                "state": r.state,
                "label": r.label,
                "path": r.path,
                "drive_id": r.drive_id,
                "size": size_opt,
            });
            if let Some(a) = &r.assignment {
                obj["assignment"] = assignment_to_json(a);
            }
            if let Some(tree) = &tree {
                let parts_list: Vec<Value> = if let Some(cid) = &r.catalog_id
                    && let Some(eid) = r.entry_id
                    && let Some(loaded) = set.get(cid)
                {
                    catalog::parts(&loaded.catalog, tree, eid)
                        .into_iter()
                        .map(|(id, role)| json!({"entry_id": id, "role": role.as_str()}))
                        .collect()
                } else if let Some(a) = &r.assignment
                    && let Some(eid) = r.entry_id
                    && let Some(role) = a.role
                {
                    vec![json!({"entry_id": eid, "role": role.as_str()})]
                } else {
                    Vec::new()
                };
                obj["parts"] = Value::Array(parts_list);
            }
            matches_json.push(obj);
        }

        let mut by_catalog: Vec<Value> = by_catalog_map
            .into_iter()
            .map(|(cid, (app, count, size))| {
                json!({
                    "catalog": cid,
                    "app": app,
                    "count": count,
                    "size": totals.then_some(size),
                })
            })
            .collect();
        by_catalog.sort_by(|a, b| {
            let key_a = (
                a["size"].as_u64().unwrap_or(0),
                a["count"].as_u64().unwrap_or(0),
            );
            let key_b = (
                b["size"].as_u64().unwrap_or(0),
                b["count"].as_u64().unwrap_or(0),
            );
            key_b
                .cmp(&key_a)
                .then_with(|| a["catalog"].as_str().cmp(&b["catalog"].as_str()))
        });

        let mut by_app: Vec<Value> = by_app_map
            .into_iter()
            .map(|(app, (count, size))| {
                json!({
                    "app": app,
                    "count": count,
                    "size": totals.then_some(size),
                })
            })
            .collect();
        by_app.sort_by(|a, b| {
            let key_a = (
                a["size"].as_u64().unwrap_or(0),
                a["count"].as_u64().unwrap_or(0),
            );
            let key_b = (
                b["size"].as_u64().unwrap_or(0),
                b["count"].as_u64().unwrap_or(0),
            );
            key_b
                .cmp(&key_a)
                .then_with(|| a["app"].as_str().cmp(&b["app"].as_str()))
        });

        Ok(json!({
            "total": total,
            "shown": matches_json.len(),
            "matches": matches_json,
            "by_catalog": by_catalog,
            "by_app": by_app,
        }))
    }

    /// どの型にも仕分けにも当たらない、大きなフォルダを探す。
    pub(super) fn unclassified(&self, args: &Value) -> Result<Value, String> {
        let mut db = self.db()?;
        let min_size = args
            .get("min_size")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MIN_SIZE);
        let limit = arg_usize(args, "limit", DEFAULT_UNCLASSIFIED_LIMIT).min(MAX_LIMIT);

        let under_loc = match args.get("under").and_then(Value::as_str) {
            Some(path) => {
                let loc = query::locate(&db, Path::new(path))
                    .map_err(err)?
                    .ok_or_else(|| {
                        format!(
                            "{path} は DB にありません（まだ読んでいないか、Lisa が読むドライブの外です）"
                        )
                    })?;
                let id = loc.chain[loc.chain.len() - 1];
                let item = query::item(&db, id).map_err(err)?.ok_or("消えました")?;
                if item.kind != "dir" {
                    return Err(format!("フォルダを指定してください: {path}"));
                }
                Some((id, loc.path))
            }
            None => None,
        };
        let under_id = under_loc.as_ref().map(|(id, _)| *id);

        let mut matched: HashSet<i64> = catalog_db::matches(&db)
            .map_err(err)?
            .into_iter()
            .map(|m| m.entry_id)
            .collect();
        for (_, eid) in catalog_db::resolve_assignments(&mut db).map_err(err)? {
            if let Some(id) = eid {
                matched.insert(id);
            }
        }

        let found =
            catalog_db::unclassified(&db, &matched, under_id, min_size, limit).map_err(err)?;
        let mut folders = Vec::with_capacity(found.len());
        for (entry_id, size) in found {
            let Some(loc) = query::path_of(&db, entry_id).map_err(err)? else {
                continue;
            };
            let name = query::item(&db, entry_id)
                .map_err(err)?
                .map(|i| i.name)
                .unwrap_or_default();
            folders.push(json!({
                "entry_id": entry_id,
                "name": name,
                "path": loc.path.to_string_lossy(),
                "drive_id": loc.drive_id,
                "size": size,
            }));
        }

        let mut out = json!({
            "min_size": min_size,
            "count": folders.len(),
            "folders": folders,
        });
        if let Some((_, path)) = under_loc {
            out["under"] = json!(path.to_string_lossy());
        }
        Ok(out)
    }

    /// フォルダの形の要約（木、拡張子統計、頻出名、小さなテキストの先頭、当たっている型と仕分け）。
    pub(super) fn outline(&self, args: &Value) -> Result<Value, String> {
        let mut db = self.db()?;
        let target = self.target(&db, args)?;
        let target_id = target.chain[target.chain.len() - 1];
        let item = query::item(&db, target_id)
            .map_err(err)?
            .ok_or("消えました")?;
        if item.kind != "dir" {
            return Err("フォルダを指定してください".into());
        }

        let max_depth = arg_usize(args, "depth", DEFAULT_OUTLINE_DEPTH).clamp(1, MAX_OUTLINE_DEPTH);
        let text_bytes = arg_usize(args, "text_bytes", DEFAULT_TEXT_BYTES).min(MAX_TEXT_BYTES);

        // BFS でサブツリーを読み、統計・木（max_depth 段まで）・テキストサンプル候補を集める。
        let mut visited: HashSet<i64> = HashSet::new();
        visited.insert(target_id);
        let mut queue: VecDeque<(i64, usize, String)> = VecDeque::new();
        queue.push_back((target_id, 0, String::new()));

        let mut total_files: u64 = 0;
        let mut total_dirs: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut tree_children: HashMap<i64, Vec<Stored>> = HashMap::new();
        let mut ext_stats: HashMap<String, (u64, u64)> = HashMap::new();
        let mut name_counts: HashMap<String, u64> = HashMap::new();
        let mut text_candidates: Vec<(i64, String, u64)> = Vec::new();

        while let Some((dir_id, depth, rel_prefix)) = queue.pop_front() {
            let mut kids = db.children(dir_id).map_err(err)?;
            kids.sort_by(|a, b| a.name_nfc.cmp(&b.name_nfc));

            if depth < max_depth {
                tree_children.insert(dir_id, kids.clone());
            }

            for child in kids {
                *name_counts.entry(child.name_nfc.clone()).or_insert(0) += 1;
                let rel = if rel_prefix.is_empty() {
                    child.name_nfc.clone()
                } else {
                    format!("{rel_prefix}/{}", child.name_nfc)
                };
                match child.kind {
                    Kind::Dir => {
                        total_dirs += 1;
                        if visited.insert(child.id) {
                            queue.push_back((child.id, depth + 1, rel));
                        }
                    }
                    Kind::File => {
                        total_files += 1;
                        total_bytes += child.size;
                        let ext = file_ext_lower(&child.name_nfc);
                        let stat = ext_stats.entry(ext.clone()).or_insert((0, 0));
                        stat.0 += 1;
                        stat.1 += child.size;
                        if text_bytes > 0
                            && !child.cloud_only
                            && child.size <= MAX_TEXT_FILE_SIZE
                            && TEXT_EXTENSIONS.contains(&ext.as_str())
                            && text_candidates.len() < MAX_TEXT_SAMPLES * 3
                        {
                            text_candidates.push((child.id, rel, child.size));
                        }
                    }
                    Kind::Symlink | Kind::Other => {}
                }
            }
        }

        let tree_json = build_outline_tree(target_id, &item.name, 0, max_depth, &tree_children);

        let mut extensions: Vec<(String, u64, u64)> = ext_stats
            .into_iter()
            .map(|(ext, (count, bytes))| (ext, count, bytes))
            .collect();
        extensions.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
        extensions.truncate(OUTLINE_TOP_STATS);
        let extensions_json: Vec<Value> = extensions
            .into_iter()
            .map(|(ext, count, bytes)| json!({"ext": ext, "count": count, "bytes": bytes}))
            .collect();

        let mut top_names: Vec<(String, u64)> = name_counts.into_iter().collect();
        top_names.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top_names.truncate(OUTLINE_TOP_STATS);
        let top_names_json: Vec<Value> = top_names
            .into_iter()
            .map(|(name, count)| json!({"name": name, "count": count}))
            .collect();

        let mut text_samples = Vec::new();
        if text_bytes > 0 {
            for (file_id, rel, size) in text_candidates {
                if text_samples.len() >= MAX_TEXT_SAMPLES {
                    break;
                }
                let Some(loc) = query::path_of(&db, file_id).map_err(err)? else {
                    continue;
                };
                if let Ok(buf) = read_file_head(&loc.path, text_bytes) {
                    let truncated = size > buf.len() as u64;
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    text_samples.push(json!({
                        "path": rel,
                        "size": size,
                        "truncated": truncated,
                        "text": text,
                    }));
                }
            }
        }

        // すでに当たっている型と仕分け。
        let (_, set) = self.load_catalog_set();
        let matched_catalogs: Vec<Value> = catalog_db::matches(&db)
            .map_err(err)?
            .into_iter()
            .filter(|m| m.entry_id == target_id)
            .map(|m| {
                let loaded = set.get(&m.catalog_id);
                let label = loaded
                    .map(|l| l.catalog.label_text(&m.captures))
                    .unwrap_or_else(|| m.catalog_id.clone());
                json!({
                    "catalog": m.catalog_id,
                    "app": loaded.map(|l| l.catalog.app.as_str()),
                    "state": state_str(m.state),
                    "label": label,
                })
            })
            .collect();

        let matched_assignments: Vec<Value> = catalog_db::resolve_assignments(&mut db)
            .map_err(err)?
            .into_iter()
            .filter(|(_, eid)| *eid == Some(target_id))
            .map(|(a, _)| assignment_to_json(&a))
            .collect();

        Ok(json!({
            "entry_id": target_id,
            "path": target.path.to_string_lossy(),
            "drive_id": target.drive_id,
            "totals": {
                "files": total_files,
                "folders": total_dirs,
                "bytes": total_bytes,
            },
            "tree": tree_json,
            "extensions": extensions_json,
            "top_names": top_names_json,
            "text_samples": text_samples,
            "matches": matched_catalogs,
            "assignments": matched_assignments,
        }))
    }

    /// 書きかけのカタログ（TOML）を保存せずに DB に当て、当たりと惜しい候補を返す。
    pub(super) fn catalog_test(&self, args: &Value) -> Result<Value, String> {
        let toml_str = args
            .get("toml")
            .and_then(Value::as_str)
            .ok_or("toml が要ります")?;
        let cat = catalog::parse(toml_str)?;
        let limit = arg_usize(args, "limit", DEFAULT_TEST_LIMIT).min(MAX_LIMIT);

        let db = self.db()?;
        let under = match args.get("under").and_then(Value::as_str) {
            Some(path) => {
                let loc = query::locate(&db, Path::new(path))
                    .map_err(err)?
                    .ok_or_else(|| {
                        format!(
                            "{path} は DB にありません（まだ読んでいないか、Lisa が読むドライブの外です）"
                        )
                    })?;
                let id = loc.chain[loc.chain.len() - 1];
                Some((loc.drive_id, id))
            }
            None => None,
        };
        let drive_filter = under.as_ref().map(|(d, _)| d.as_str());
        let under_id = under.as_ref().map(|(_, id)| *id);

        let set = single_catalog_set(cat.clone(), "test:inline");
        let (found, _) = catalog_db::find(&db, &set, drive_filter, false, &|| true)
            .map_err(err)?
            .ok_or("中断されました")?;
        let nears = catalog_db::find_near_misses(&db, &set, drive_filter, false, &|| true)
            .map_err(err)?
            .ok_or("中断されました")?;

        let mut matches_json = Vec::new();
        let mut matches_total = 0usize;
        for f in found {
            let Some(loc) = query::path_of(&db, f.entry_id).map_err(err)? else {
                continue;
            };
            if under_id.is_some_and(|uid| !loc.chain.contains(&uid)) {
                continue;
            }
            matches_total += 1;
            if matches_json.len() < limit {
                matches_json.push(json!({
                    "entry_id": f.entry_id,
                    "path": loc.path.to_string_lossy(),
                    "drive_id": loc.drive_id,
                    "state": state_str(f.state),
                    "label": cat.label_text(&f.captures),
                    "captures": f.captures,
                }));
            }
        }

        let mut near_json = Vec::new();
        let mut near_total = 0usize;
        for n in nears {
            let Some(loc) = query::path_of(&db, n.entry_id).map_err(err)? else {
                continue;
            };
            if under_id.is_some_and(|uid| !loc.chain.contains(&uid)) {
                continue;
            }
            near_total += 1;
            if near_json.len() < limit {
                let failed = describe_failed_match(&cat, n.failed_match);
                near_json.push(json!({
                    "entry_id": n.entry_id,
                    "path": loc.path.to_string_lossy(),
                    "drive_id": loc.drive_id,
                    "failed_match": n.failed_match,
                    "failed": failed,
                }));
            }
        }

        Ok(json!({
            "id": cat.id,
            "name": cat.name,
            "app": cat.app,
            "matches_total": matches_total,
            "matches": matches_json,
            "near_total": near_total,
            "near": near_json,
        }))
    }

    /// カタログの TOML ファイルをユーザーのカタログのフォルダに保存する。
    pub(super) fn catalog_save(&self, args: &Value) -> Result<Value, String> {
        let toml_str = args
            .get("toml")
            .and_then(Value::as_str)
            .ok_or("toml が要ります")?;
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let cat = catalog::parse(toml_str)?;
        validate_save_id(&cat.id)?;

        let settings = self
            .settings_path()
            .map(|p| CatalogSettings::from_file(&p))
            .unwrap_or_default();
        let allowed_dirs = catalog::dirs(&self.data_dir, &settings);
        let target_dir = resolve_save_dir(
            args.get("dir").and_then(Value::as_str),
            &self.data_dir,
            &allowed_dirs,
        )?;

        ensure_safe_dir(&target_dir)?;

        let dest_path = target_dir.join(format!("{}.toml", cat.id));
        match std::fs::symlink_metadata(&dest_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err("シンボリックリンクの先には書けません".into());
                }
                if meta.is_dir() {
                    return Err(format!(
                        "同じ名前のフォルダがあります: {}",
                        dest_path.display()
                    ));
                }
                if !overwrite {
                    return Err(format!(
                        "すでに同じ名前のファイルがあります（上書きするには overwrite: true）: {}",
                        dest_path.display()
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", dest_path.display())),
        }

        // 一時ファイル（.toml で終わらない名前）に書いてから rename する。
        let tmp_path = target_dir.join(format!(".{}.toml.tmp.{}", cat.id, std::process::id()));
        if let Err(e) = std::fs::write(&tmp_path, toml_str.as_bytes()) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("{}: {e}", tmp_path.display()));
        }
        if let Err(e) = std::fs::rename(&tmp_path, &dest_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("{}: {e}", dest_path.display()));
        }

        let match_count = match self.db_if_exists()? {
            Some(db) => {
                let set = single_catalog_set(cat.clone(), dest_path.clone());
                let (found, _) = catalog_db::find(&db, &set, None, false, &|| true)
                    .map_err(err)?
                    .ok_or("中断されました")?;
                found.len()
            }
            None => 0,
        };

        Ok(json!({
            "saved": true,
            "id": cat.id,
            "path": dest_path.to_string_lossy(),
            "matches": match_count,
        }))
    }

    /// 1 か所の仕分けを DB の `assignment` に書く（同じ drive_id + path なら上書き）。
    pub(super) fn assign(&self, args: &Value) -> Result<Value, String> {
        let mut db = self.db()?;
        let target = self.target(&db, args)?;
        let entry_id = target.chain[target.chain.len() - 1];
        let item = query::item(&db, entry_id)
            .map_err(err)?
            .ok_or("消えました")?;
        if item.kind != "dir" {
            return Err("フォルダを指定してください".into());
        }

        let app = args
            .get("app")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("app が要ります")?;
        let role = match args.get("role").and_then(Value::as_str) {
            Some(r) => Some(Role::from_name(r).ok_or_else(|| {
                format!("知らない role です（data, config, cache, install, log のどれか）: {r}")
            })?),
            None => None,
        };
        let note = args.get("note").and_then(Value::as_str);

        let a = catalog_db::assign(&mut db, entry_id, app, role, note, "mcp").map_err(err)?;
        Ok(json!({
            "assignment": assignment_to_json(&a),
            "entry_id": entry_id,
            "full_path": target.path.to_string_lossy(),
        }))
    }

    /// 1 か所の仕分けの行を DB から消す（ユーザーのファイルは触らない）。
    pub(super) fn unassign(&self, args: &Value) -> Result<Value, String> {
        let mut db = self.db()?;
        if let Some(aid) = args.get("assignment_id").and_then(Value::as_i64) {
            let removed = catalog_db::unassign(&mut db, aid).map_err(err)?;
            return Ok(json!({
                "removed": removed,
                "assignment_id": aid,
            }));
        }

        match self.target(&db, args) {
            Ok(target) => {
                let entry_id = target.chain[target.chain.len() - 1];
                let resolved = catalog_db::resolve_assignments(&mut db).map_err(err)?;
                if let Some((a, _)) = resolved.into_iter().find(|(_, eid)| *eid == Some(entry_id)) {
                    let removed = catalog_db::unassign(&mut db, a.id).map_err(err)?;
                    return Ok(json!({
                        "removed": removed,
                        "assignment_id": a.id,
                        "path": target.path.to_string_lossy(),
                    }));
                }
                Ok(json!({
                    "removed": false,
                    "path": target.path.to_string_lossy(),
                }))
            }
            Err(target_err) => {
                // フォルダが消えて entry が無くなった仕分けも、パスが一致すれば消せるようにする。
                if let Some(path_str) = args.get("path").and_then(Value::as_str)
                    && let Some(aid) = find_assignment_by_full_path(&db, Path::new(path_str))?
                {
                    let removed = catalog_db::unassign(&mut db, aid).map_err(err)?;
                    return Ok(json!({
                        "removed": removed,
                        "assignment_id": aid,
                        "path": path_str,
                    }));
                }
                Err(target_err)
            }
        }
    }
}

struct MatchCandidate {
    entry_id: Option<i64>,
    catalog_id: Option<String>,
    app: String,
    state: &'static str,
    label: String,
    path: String,
    drive_id: String,
    assignment: Option<catalog_db::Assignment>,
}

fn state_str(state: State) -> &'static str {
    match state {
        State::Match => "match",
        State::Maybe => "maybe",
        State::Pending => "pending",
        State::Miss => "miss",
    }
}

fn single_catalog_set(cat: Catalog, source: impl Into<PathBuf>) -> CatalogSet {
    CatalogSet {
        catalogs: vec![Loaded {
            catalog: cat,
            source: source.into(),
        }],
        errors: Vec::new(),
    }
}

fn read_catalog_text(source: &Path) -> Option<String> {
    if let Some(name) = source.to_str().and_then(|s| s.strip_prefix("builtin:")) {
        return catalog::builtin()
            .iter()
            .find(|&&(n, _)| n == name)
            .map(|&(_, text)| text.to_string());
    }
    std::fs::read_to_string(source).ok()
}

fn catalog_summary(loaded: &Loaded, counts: &HashMap<String, usize>, include_text: bool) -> Value {
    let cat = &loaded.catalog;
    let [r, g, b] = cat.color_rgb();
    let color = format!("#{r:02x}{g:02x}{b:02x}");
    let mut obj = json!({
        "id": cat.id,
        "name": cat.name,
        "app": cat.app,
        "source": loaded.source.to_string_lossy(),
        "rev": cat.rev,
        "matches": counts.get(&cat.id).copied().unwrap_or(0),
        "label": cat.label,
        "color": color,
    });
    if include_text {
        obj["text"] = json!(read_catalog_text(&loaded.source));
    }
    obj
}

fn assignment_to_json(a: &catalog_db::Assignment) -> Value {
    json!({
        "id": a.id,
        "drive_id": a.drive_id,
        "path": a.path,
        "file_id": a.file_id,
        "app": a.app,
        "role": a.role.map(Role::as_str),
        "note": a.note,
        "by": a.by,
        "created_at": a.created_at,
        "lost_at": a.lost_at,
    })
}

fn file_ext_lower(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

fn read_file_head(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut buf = Vec::with_capacity(limit.min(4096));
    file.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn build_outline_tree(
    id: i64,
    name: &str,
    depth: usize,
    max_depth: usize,
    children_map: &HashMap<i64, Vec<Stored>>,
) -> Value {
    let mut node = json!({
        "name": name,
        "kind": "dir",
    });
    if depth >= max_depth {
        return node;
    }
    let Some(kids) = children_map.get(&id) else {
        return node;
    };
    let total = kids.len();
    let shown: Vec<Value> = kids
        .iter()
        .take(OUTLINE_CHILDREN_PER_LEVEL)
        .map(|c| match c.kind {
            Kind::Dir => build_outline_tree(c.id, &c.name_nfc, depth + 1, max_depth, children_map),
            Kind::File => json!({
                "name": c.name_nfc,
                "kind": "file",
                "size": c.size,
            }),
            Kind::Symlink => json!({
                "name": c.name_nfc,
                "kind": "symlink",
            }),
            Kind::Other => json!({
                "name": c.name_nfc,
                "kind": "other",
            }),
        })
        .collect();
    node["children"] = Value::Array(shown);
    if total > OUTLINE_CHILDREN_PER_LEVEL {
        node["more"] = json!(total - OUTLINE_CHILDREN_PER_LEVEL);
    }
    node
}

fn describe_failed_match(cat: &Catalog, failed_idx: usize) -> String {
    match cat.matches.get(failed_idx) {
        Some(rule) => {
            let pat = catalog_db::pattern_text(&rule.path);
            if rule.content.is_some() {
                format!("match[{failed_idx}] {pat} が無いか条件に合わない")
            } else {
                format!("match[{failed_idx}] {pat} が無い")
            }
        }
        None => format!("match[{failed_idx}] が外れた"),
    }
}

fn validate_save_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id == "."
        || id.contains("..")
        || id.contains('/')
        || id.contains('\\')
        || !id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
    {
        return Err(format!("id がファイル名として使えません: {id}"));
    }
    Ok(())
}

fn has_parent_dir_component(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

fn resolve_save_dir(
    dir_arg: Option<&str>,
    data_dir: &Path,
    allowed_dirs: &[PathBuf],
) -> Result<PathBuf, String> {
    let Some(dir_str) = dir_arg else {
        return Ok(catalog::user_dir(data_dir));
    };
    if dir_str.starts_with("builtin:") {
        return Err("組み込みのカタログ（builtin:）には書き込めません".into());
    }
    let req = Path::new(dir_str);
    if has_parent_dir_component(req) {
        return Err(format!("dir に .. は使えません: {dir_str}"));
    }
    if let Ok(meta) = std::fs::symlink_metadata(req)
        && meta.file_type().is_symlink()
    {
        return Err(format!("シンボリックリンクには書けません: {dir_str}"));
    }

    let req_canon = std::fs::canonicalize(req).ok();
    for allowed in allowed_dirs {
        if req == allowed {
            return Ok(allowed.clone());
        }
        if let Some(rc) = &req_canon
            && let Ok(ac) = std::fs::canonicalize(allowed)
            && rc == &ac
        {
            return Ok(allowed.clone());
        }
    }

    Err(format!(
        "書く先がユーザーのカタログのフォルダではありません: {dir_str}"
    ))
}

fn ensure_safe_dir(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "シンボリックリンクには書けません: {}",
                    dir.display()
                ));
            }
            if !meta.is_dir() {
                return Err(format!("フォルダではありません: {}", dir.display()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let meta =
                std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(format!(
                    "安全な書き込み先のフォルダを作れませんでした: {}",
                    dir.display()
                ));
            }
        }
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    Ok(())
}

fn find_assignment_by_full_path(db: &Db, path: &Path) -> Result<Option<i64>, String> {
    let drives = query::drives(db).map_err(err)?;
    let list = catalog_db::assignments(db).map_err(err)?;
    for drive in &drives {
        let Some(mount) = &drive.mount else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(mount) else {
            continue;
        };
        let rel_str = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        if let Some(a) = list
            .iter()
            .find(|a| a.drive_id == drive.id && a.path == rel_str)
        {
            return Ok(Some(a.id));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use lisa_core::listing::read_dir;
    use lisa_core::volumes::Volume;

    static NEXT_TEST_ID: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let n = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("lisa-mcp-cat-{name}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn volume(id: &str, mount: &Path) -> Volume {
        Volume {
            id: Some(id.into()),
            label: "テスト".into(),
            fs: "apfs".into(),
            mount_path: mount.to_path_buf(),
            capacity: Some(1_000_000),
            available: Some(500_000),
            removable: false,
            network: false,
            read_only: false,
            case_sensitive: Some(false),
            hidden: false,
        }
    }

    fn save_tree(db: &mut Db, drive_id: &str, mount: &Path, dir: &Path) {
        let res = read_dir(dir).map_err(|e| e.to_string());
        db.save_listing(drive_id, mount, dir, &res).unwrap();
        if let Ok(listing) = res {
            for e in listing.entries {
                if e.kind == Kind::Dir && !e.other_volume && !e.cloud_only {
                    save_tree(db, drive_id, mount, &dir.join(&e.name));
                }
            }
        }
    }

    fn call_tool(server: &Server, name: &str, args: Value) -> Value {
        server
            .handle(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": name, "arguments": args},
            }))
            .unwrap()
    }

    #[test]
    fn lisa_catalogs_lists_builtin_user_and_errors() {
        let data = TempDir::new("catalogs");
        let extra_dir = TempDir::new("extra-cats");

        // ユーザーフォルダに正常な型と壊れた TOML を置く。
        let user_cats = catalog::user_dir(&data.0);
        fs::create_dir_all(&user_cats).unwrap();
        fs::write(
            user_cats.join("my-tool.toml"),
            "id = \"my-tool\"\nname = \"My Tool\"\napp = \"MyApp\"\n[[match]]\npath = \"MyMarker.txt\"\n",
        )
        .unwrap();
        fs::write(
            user_cats.join("broken.toml"),
            "id = \"broken\"\nnot valid = [",
        )
        .unwrap();

        // settings.json で追加フォルダも指定する。
        fs::write(
            data.0.join("settings.json"),
            json!({"catalog_dirs": [extra_dir.0]}).to_string(),
        )
        .unwrap();
        fs::write(
            extra_dir.0.join("extra-tool.toml"),
            "id = \"extra-tool\"\nname = \"Extra\"\napp = \"ExtraApp\"\n[[match]]\npath = \"Extra/\"\n",
        )
        .unwrap();

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        let reply = call_tool(&server, "lisa_catalogs", json!({"include_text": true}));
        assert_ne!(reply["result"]["isError"], true);
        let sc = &reply["result"]["structuredContent"];

        assert_eq!(sc["dirs"].as_array().unwrap().len(), 2);
        let cats = sc["catalogs"].as_array().unwrap();
        assert!(cats.iter().any(|c| c["id"] == "unity-project"
            && c["source"].as_str().unwrap().starts_with("builtin:")
            && c["text"].as_str().unwrap().contains("unity-project")));
        assert!(cats.iter().any(|c| c["id"] == "my-tool"
            && c["app"] == "MyApp"
            && c["color"].as_str().unwrap().starts_with('#')
            && c["text"].as_str().unwrap().contains("MyMarker.txt")));
        assert!(cats.iter().any(|c| c["id"] == "extra-tool"));

        let errors = sc["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]["source"]
                .as_str()
                .unwrap()
                .contains("broken.toml")
        );
    }

    #[test]
    fn lisa_catalog_test_returns_matches_near_and_rejects_broken_toml() {
        let data = TempDir::new("test-tool");
        let disk = TempDir::new("test-disk");

        let hit_dir = disk.0.join("MyUnity");
        fs::create_dir_all(hit_dir.join("Assets")).unwrap();
        fs::create_dir_all(hit_dir.join("ProjectSettings")).unwrap();
        fs::write(
            hit_dir.join("ProjectSettings/ProjectVersion.txt"),
            b"m_EditorVersion: 6000.0.1f1\n",
        )
        .unwrap();

        let near_dir = disk.0.join("AlmostUnity");
        fs::create_dir_all(near_dir.join("Assets")).unwrap();

        let mut db = Db::open(&data.0.join("lisa.db")).unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("drv:1", &disk.0)).unwrap();
        save_tree(&mut db, "drv:1", &disk.0, &disk.0);
        drop(db);

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        let toml = r#"
id = "custom-unity"
name = "Custom Unity"
app = "Unity"
label = "Unity {ver}"

[[match]]
path = "Assets/"

[[match]]
path = "ProjectSettings/ProjectVersion.txt"
content = 're:m_EditorVersion: (?<ver>\S+)'
"#;
        let reply = call_tool(&server, "lisa_catalog_test", json!({"toml": toml}));
        assert_ne!(reply["result"]["isError"], true);
        let sc = &reply["result"]["structuredContent"];
        assert_eq!(sc["matches_total"], 1);
        assert_eq!(sc["matches"][0]["label"], "Unity 6000.0.1f1");
        assert!(
            sc["matches"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("MyUnity")
        );
        assert_eq!(sc["near_total"], 1);
        assert!(
            sc["near"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("AlmostUnity")
        );
        assert!(
            sc["near"][0]["failed"]
                .as_str()
                .unwrap()
                .contains("match[1]")
        );

        // content_check にも catalog_match にも書いていないこと。
        let db = Db::open(&data.0.join("lisa.db")).unwrap();
        assert!(catalog_db::matches(&db).unwrap().is_empty());

        // 壊れた TOML はエラーの文を返す。
        let bad = call_tool(&server, "lisa_catalog_test", json!({"toml": "id = 123"}));
        assert_eq!(bad["result"]["isError"], true);
        assert!(
            !bad["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lisa_catalog_save_writes_safely_and_validates_paths() {
        let data = TempDir::new("save-data");
        let outside = TempDir::new("save-outside");
        let disk = TempDir::new("save-disk");

        let proj = disk.0.join("Demo");
        fs::create_dir_all(proj.join("MyDir")).unwrap();
        let mut db = Db::open(&data.0.join("lisa.db")).unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("drv:1", &disk.0)).unwrap();
        save_tree(&mut db, "drv:1", &disk.0, &disk.0);
        drop(db);

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        let valid_toml = "id = \"my-custom\"\nname = \"My Custom\"\napp = \"CustomApp\"\n[[match]]\npath = \"MyDir/\"\n";
        // 1. 新規保存（既定の user_dir）。
        let ok = call_tool(&server, "lisa_catalog_save", json!({"toml": valid_toml}));
        assert_ne!(ok["result"]["isError"], true);
        let sc = &ok["result"]["structuredContent"];
        assert_eq!(sc["saved"], true);
        assert_eq!(sc["matches"], 1);
        let saved_path = PathBuf::from(sc["path"].as_str().unwrap());
        assert_eq!(
            saved_path,
            catalog::user_dir(&data.0).join("my-custom.toml")
        );
        assert!(saved_path.exists());

        // 2. overwrite なしで同じ名前は断る。
        let dup = call_tool(&server, "lisa_catalog_save", json!({"toml": valid_toml}));
        assert_eq!(dup["result"]["isError"], true);

        // 3. overwrite: true なら上書きできる。
        let updated_toml = "id = \"my-custom\"\nname = \"My Custom v2\"\napp = \"CustomApp\"\n[[match]]\npath = \"MyDir/\"\n";
        let ov = call_tool(
            &server,
            "lisa_catalog_save",
            json!({"toml": updated_toml, "overwrite": true}),
        );
        assert_ne!(ov["result"]["isError"], true);
        assert!(
            fs::read_to_string(&saved_path)
                .unwrap()
                .contains("My Custom v2")
        );

        // 4. `../x` や `a..b` のような id は断る。
        for bad_id in ["../x", "a..b", "..", "."] {
            let bad_toml = format!(
                "id = \"{bad_id}\"\nname = \"Bad\"\napp = \"Bad\"\n[[match]]\npath = \"MyDir/\"\n"
            );
            let res = call_tool(&server, "lisa_catalog_save", json!({"toml": bad_toml}));
            assert_eq!(res["result"]["isError"], true, "bad_id={bad_id}");
        }

        // 5. ユーザーのフォルダ以外の dir や builtin: は断る。
        let out_res = call_tool(
            &server,
            "lisa_catalog_save",
            json!({"toml": valid_toml, "dir": outside.0}),
        );
        assert_eq!(out_res["result"]["isError"], true);
        assert!(!outside.0.join("my-custom.toml").exists());

        let builtin_res = call_tool(
            &server,
            "lisa_catalog_save",
            json!({"toml": valid_toml, "dir": "builtin:catalogs"}),
        );
        assert_eq!(builtin_res["result"]["isError"], true);

        // 6. 壊れた TOML は書かない。
        let broken = call_tool(
            &server,
            "lisa_catalog_save",
            json!({"toml": "id = \"never-written\"\nname = \"X\"\n"}),
        );
        assert_eq!(broken["result"]["isError"], true);
        assert!(
            !catalog::user_dir(&data.0)
                .join("never-written.toml")
                .exists()
        );
    }

    #[test]
    fn lisa_assign_unassign_and_catalog_matches() {
        let data = TempDir::new("assign-data");
        let disk = TempDir::new("assign-disk");

        let custom_dir = disk.0.join("CustomSaves");
        fs::create_dir_all(&custom_dir).unwrap();
        fs::write(custom_dir.join("save.dat"), vec![0u8; 4096]).unwrap();

        let node_dir = disk.0.join("WebApp");
        fs::create_dir_all(node_dir.join("node_modules/pkg")).unwrap();
        let pkg_json = br#"{"name": "web-app"}"#;
        fs::write(node_dir.join("package.json"), pkg_json).unwrap();
        fs::write(node_dir.join("node_modules/pkg/index.js"), vec![0u8; 1024]).unwrap();

        let mut db = Db::open(&data.0.join("lisa.db")).unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("drv:1", &disk.0)).unwrap();
        save_tree(&mut db, "drv:1", &disk.0, &disk.0);

        let set = CatalogSet::load_with_builtin(&[]);
        let (found, _) = catalog_db::find(&db, &set, None, false, &|| true)
            .unwrap()
            .unwrap();
        catalog_db::save(&mut db, &set, &found, None).unwrap();
        drop(db);

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        // 1. lisa_assign で仕分けを付ける。
        let assigned = call_tool(
            &server,
            "lisa_assign",
            json!({
                "path": custom_dir,
                "app": "MyGame",
                "role": "data",
                "note": "セーブデータ",
            }),
        );
        assert_ne!(assigned["result"]["isError"], true);
        let aid = assigned["result"]["structuredContent"]["assignment"]["id"]
            .as_i64()
            .unwrap();
        assert_eq!(
            assigned["result"]["structuredContent"]["assignment"]["by"],
            "mcp"
        );
        assert_eq!(
            assigned["result"]["structuredContent"]["assignment"]["path"],
            "CustomSaves"
        );

        // 2. lisa_catalog_matches に型と仕分けの両方が出て、parts: true で役割が付く。
        let matches = call_tool(&server, "lisa_catalog_matches", json!({"parts": true}));
        assert_ne!(matches["result"]["isError"], true);
        let sc = &matches["result"]["structuredContent"];
        assert_eq!(sc["total"], 2);
        let list = sc["matches"].as_array().unwrap();

        let assign_row = list.iter().find(|r| r["catalog"].is_null()).unwrap();
        assert_eq!(assign_row["app"], "MyGame");
        assert_eq!(assign_row["size"], 4096);
        assert_eq!(assign_row["assignment"]["role"], "data");
        assert_eq!(assign_row["parts"][0]["role"], "data");

        let node_row = list
            .iter()
            .find(|r| r["catalog"] == "node-package")
            .unwrap();
        assert_eq!(node_row["size"], 1043);
        assert_eq!(node_row["label"], "web-app");
        assert!(!node_row["parts"].as_array().unwrap().is_empty());

        // 3. lisa_unassign で消える。
        let un = call_tool(&server, "lisa_unassign", json!({"assignment_id": aid}));
        assert_ne!(un["result"]["isError"], true);
        assert_eq!(un["result"]["structuredContent"]["removed"], true);

        let after = call_tool(&server, "lisa_catalog_matches", json!({}));
        assert_eq!(after["result"]["structuredContent"]["total"], 1);
    }

    #[test]
    fn lisa_unclassified_and_lisa_outline_return_expected_shape() {
        let data = TempDir::new("outline-data");
        let disk = TempDir::new("outline-disk");

        // 未分類の大きなフォルダ（big）と小さなフォルダ（small）、型に当たったフォルダ（matched）。
        let big = disk.0.join("parent/big");
        fs::create_dir_all(&big).unwrap();
        fs::write(big.join("readme.txt"), b"hello world\nline2").unwrap();
        fs::write(big.join("archive.bin"), vec![0u8; 5000]).unwrap();
        fs::create_dir_all(big.join("sub")).unwrap();
        for i in 0..45 {
            fs::write(big.join(format!("sub/f{i:02}.dat")), vec![0u8; 10]).unwrap();
        }

        let small = disk.0.join("parent/small");
        fs::create_dir_all(&small).unwrap();
        fs::write(small.join("tiny.txt"), b"tiny").unwrap();

        let matched = disk.0.join("MyRepo");
        fs::create_dir_all(matched.join(".git")).unwrap();
        fs::write(matched.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(matched.join("blob.bin"), vec![0u8; 8000]).unwrap();

        let mut db = Db::open(&data.0.join("lisa.db")).unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("drv:1", &disk.0)).unwrap();
        save_tree(&mut db, "drv:1", &disk.0, &disk.0);

        let set = CatalogSet::load_with_builtin(&[]);
        let (found, _) = catalog_db::find(&db, &set, None, false, &|| true)
            .unwrap()
            .unwrap();
        catalog_db::save(&mut db, &set, &found, None).unwrap();
        drop(db);

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        // 1. lisa_unclassified: 型が当たった MyRepo は除かれ、big だけが出る。
        let unc = call_tool(&server, "lisa_unclassified", json!({"min_size": 1000}));
        assert_ne!(unc["result"]["isError"], true);
        let unc_sc = &unc["result"]["structuredContent"];
        assert_eq!(unc_sc["count"], 1);
        assert!(
            unc_sc["folders"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("parent/big")
        );

        // 2. lisa_outline: 木、拡張子統計、テキスト先頭、40 件超の more が返る。
        let out = call_tool(
            &server,
            "lisa_outline",
            json!({"path": big, "depth": 3, "text_bytes": 5}),
        );
        assert_ne!(out["result"]["isError"], true);
        let out_sc = &out["result"]["structuredContent"];
        assert_eq!(out_sc["totals"]["files"], 47);
        assert_eq!(out_sc["totals"]["folders"], 1);
        let samples = out_sc["text_samples"].as_array().unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0]["path"], "readme.txt");
        assert_eq!(samples[0]["text"], "hello");
        assert_eq!(samples[0]["truncated"], true);

        let sub_node = out_sc["tree"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "sub")
            .unwrap();
        assert_eq!(sub_node["children"].as_array().unwrap().len(), 40);
        assert_eq!(sub_node["more"], 5);

        let exts = out_sc["extensions"].as_array().unwrap();
        assert_eq!(exts[0]["ext"], "bin");
        assert_eq!(exts[0]["bytes"], 5000);

        // 3. 型に当たったフォルダの lisa_outline には matches が入る。
        let out_m = call_tool(&server, "lisa_outline", json!({"path": matched}));
        assert_ne!(out_m["result"]["isError"], true);
        assert_eq!(
            out_m["result"]["structuredContent"]["matches"][0]["catalog"],
            "git-repository"
        );
    }

    #[test]
    fn lisa_outline_skips_cloud_only_and_catalog_test_respects_under() {
        let data = TempDir::new("edge-data");
        let disk = TempDir::new("edge-disk");

        let sub1 = disk.0.join("scope1/AppA");
        let sub2 = disk.0.join("scope2/AppB");
        fs::create_dir_all(&sub1).unwrap();
        fs::create_dir_all(&sub2).unwrap();
        fs::write(sub1.join("marker.txt"), b"secret cloud text").unwrap();
        fs::write(sub2.join("marker.txt"), b"other").unwrap();

        let mut db = Db::open(&data.0.join("lisa.db")).unwrap();
        let dev = db.this_device("mac", "mac.local", "macos").unwrap();
        db.upsert_drive(&dev, &volume("drv:1", &disk.0)).unwrap();
        save_tree(&mut db, "drv:1", &disk.0, &disk.0);

        // AppA/marker.txt を cloud_only = true で上書き保存して、lisa_outline が読まないことを確かめる。
        let cloud_listing = lisa_core::listing::Listing {
            entries: vec![lisa_core::listing::Entry {
                name: "marker.txt".into(),
                name_raw: b"marker.txt".to_vec(),
                name_nfc: "marker.txt".into(),
                kind: Kind::File,
                size: 17,
                mtime_ms: None,
                file_id: None,
                nlink: None,
                link_target: None,
                cloud_only: true,
                other_volume: false,
            }],
            junk: 0,
            failed: Vec::new(),
        };
        db.save_listing("drv:1", &disk.0, &sub1, &Ok(cloud_listing))
            .unwrap();
        drop(db);

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        let outline_cloud = call_tool(&server, "lisa_outline", json!({"path": sub1}));
        assert_ne!(outline_cloud["result"]["isError"], true);
        assert!(
            outline_cloud["result"]["structuredContent"]["text_samples"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let toml = "id = \"marker\"\nname = \"Marker\"\napp = \"Marker\"\n[[match]]\npath = \"marker.txt\"\n";
        let res = call_tool(
            &server,
            "lisa_catalog_test",
            json!({"toml": toml, "under": disk.0.join("scope1")}),
        );
        assert_ne!(res["result"]["isError"], true);
        let sc = &res["result"]["structuredContent"];
        assert_eq!(sc["matches_total"], 1);
        assert!(
            sc["matches"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("scope1/AppA")
        );

        // totals: false とフィルタのテスト（lisa_catalog_matches）。
        let assign_res = call_tool(
            &server,
            "lisa_assign",
            json!({"path": sub1, "app": "AppOne"}),
        );
        assert_ne!(assign_res["result"]["isError"], true);
        let m = call_tool(
            &server,
            "lisa_catalog_matches",
            json!({"app": "AppOne", "totals": false}),
        );
        assert_ne!(m["result"]["isError"], true);
        assert_eq!(m["result"]["structuredContent"]["total"], 1);
        assert!(m["result"]["structuredContent"]["matches"][0]["size"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn lisa_catalog_save_rejects_symlinks() {
        let data = TempDir::new("sym-data");
        let outside = TempDir::new("sym-outside");
        let user_cats = catalog::user_dir(&data.0);
        fs::create_dir_all(&user_cats).unwrap();

        let target_file = outside.0.join("victim.toml");
        fs::write(&target_file, "keep").unwrap();
        let link_file = user_cats.join("link-cat.toml");
        std::os::unix::fs::symlink(&target_file, &link_file).unwrap();

        let link_dir = data.0.join("link-dir");
        std::os::unix::fs::symlink(&outside.0, &link_dir).unwrap();
        fs::write(
            data.0.join("settings.json"),
            json!({"catalog_dirs": [link_dir]}).to_string(),
        )
        .unwrap();

        let server = Server {
            data_dir: data.0.clone(),
            cache_dir: None,
        };

        // 1. 出力先ファイルがシンボリックリンクなら overwrite: true でも断る。
        let toml1 = "id = \"link-cat\"\nname = \"Link\"\napp = \"Link\"\n[[match]]\npath = \"x\"\n";
        let r1 = call_tool(
            &server,
            "lisa_catalog_save",
            json!({"toml": toml1, "overwrite": true}),
        );
        assert_eq!(r1["result"]["isError"], true);
        assert_eq!(fs::read_to_string(&target_file).unwrap(), "keep");

        // 2. dir がシンボリックリンクなら断る。
        let toml2 =
            "id = \"normal-cat\"\nname = \"Normal\"\napp = \"Normal\"\n[[match]]\npath = \"x\"\n";
        let r2 = call_tool(
            &server,
            "lisa_catalog_save",
            json!({"toml": toml2, "dir": link_dir}),
        );
        assert_eq!(r2["result"]["isError"], true);
        assert!(!outside.0.join("normal-cat.toml").exists());
    }
}
