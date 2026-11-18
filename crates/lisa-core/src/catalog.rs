//! カタログ（型）の TOML 読み込みと、フォルダ木への当てはめ。
//!
//! DB や画面には依存しない純粋なロジックだけを持つ。

mod content;
pub mod db;
mod eval;
mod pattern;

mod builtin_catalogs {
    include!(concat!(env!("OUT_DIR"), "/builtin_catalogs.rs"));
}

/// ビルド時に埋め込んだ組み込みのカタログ（`(ファイル名, TOML 文字列)`）。
pub fn builtin() -> &'static [(&'static str, &'static str)] {
    builtin_catalogs::BUILTIN
}

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

pub use content::{Captures, Check, Content, FileFacts};
pub use eval::{Failed, Node, Outcome, Reader, State, Tree, evaluate, parts};
pub use pattern::{NameMatcher, Pattern, Segment};

/// 1 つのアプリやプロジェクトを表す型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub id: String,
    pub name: String,
    pub app: String,
    /// `"#rrggbb"`。省略なら None。
    pub color: Option<String>,
    /// `"Unity {ver}"` のような札の雛形。
    pub label: Option<String>,
    /// 1 つ以上（0 個は読み込みエラー）。
    pub matches: Vec<Rule>,
    pub excludes: Vec<Rule>,
    pub parts: Vec<Part>,
    /// 読み込んだ TOML（バイト列）の blake3 の 16 進文字列。
    pub rev: String,
}

/// `[[match]]` または `[[exclude]]` の 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub path: Pattern,
    pub content: Option<Content>,
}

/// `[[part]]` の 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub path: Pattern,
    pub role: Role,
}

/// 当たったフォルダの中の役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Data,
    Config,
    Cache,
    Install,
    Log,
}

impl Role {
    /// TOML や DB での書き方（`data` など）。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Data => "data",
            Role::Config => "config",
            Role::Cache => "cache",
            Role::Install => "install",
            Role::Log => "log",
        }
    }

    /// `as_str` の逆。知らない文字なら None。
    pub fn from_name(name: &str) -> Option<Role> {
        Some(match name {
            "data" => Role::Data,
            "config" => Role::Config,
            "cache" => Role::Cache,
            "install" => Role::Install,
            "log" => Role::Log,
            _ => return None,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    id: String,
    name: String,
    app: String,
    color: Option<String>,
    label: Option<String>,
    #[serde(rename = "match", default)]
    matches: Vec<RawRule>,
    #[serde(rename = "exclude", default)]
    excludes: Vec<RawRule>,
    #[serde(rename = "part", default)]
    parts: Vec<RawPart>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    path: String,
    #[serde(default)]
    case: bool,
    content: Option<String>,
    content_bytes: Option<String>,
    hash: Option<String>,
    size: Option<String>,
    head: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPart {
    path: String,
    #[serde(default)]
    case: bool,
    role: Role,
}

/// TOML の文字列から `Catalog` を読み込む。
pub fn parse(text: &str) -> Result<Catalog, String> {
    let raw: RawCatalog =
        toml::from_str(text).map_err(|e| format!("TOML の形式が正しくありません: {e}"))?;

    validate_id(&raw.id)?;
    if raw.name.trim().is_empty() {
        return Err("name が空です".to_string());
    }
    if raw.app.trim().is_empty() {
        return Err("app が空です".to_string());
    }
    if let Some(color) = &raw.color {
        validate_color(color)?;
    }

    if raw.matches.is_empty() {
        return Err("match が 1 つもありません（1 つ以上必要です）".to_string());
    }

    let mut matches = Vec::with_capacity(raw.matches.len());
    for r in raw.matches {
        matches.push(parse_rule(r, "match")?);
    }

    if matches
        .iter()
        .all(|r| matches!(r.path.first(), Segment::Any))
    {
        return Err(
            "すべての match の 1 段目が ** です（名前で絞れるものを 1 つ以上入れてください）"
                .to_string(),
        );
    }

    let mut excludes = Vec::with_capacity(raw.excludes.len());
    for r in raw.excludes {
        excludes.push(parse_rule(r, "exclude")?);
    }

    let mut parts = Vec::with_capacity(raw.parts.len());
    for p in raw.parts {
        let path = Pattern::parse(&p.path, p.case).map_err(|e| format!("part の {e}"))?;
        parts.push(Part { path, role: p.role });
    }

    let rev = blake3::hash(text.as_bytes()).to_hex().to_string();

    Ok(Catalog {
        id: raw.id,
        name: raw.name,
        app: raw.app,
        color: raw.color,
        label: raw.label,
        matches,
        excludes,
        parts,
        rev,
    })
}

fn parse_rule(raw: RawRule, section: &str) -> Result<Rule, String> {
    let path = Pattern::parse(&raw.path, raw.case).map_err(|e| format!("{section} の {e}"))?;
    let content = Content::parse(
        raw.content.as_deref(),
        raw.content_bytes.as_deref(),
        raw.hash.as_deref(),
        raw.size.as_deref(),
        raw.head.as_deref(),
    )?;
    if path.is_dir() && content.is_some() {
        return Err(format!(
            "{section} のフォルダパターンには中身の条件を付けられません: {}",
            raw.path
        ));
    }
    Ok(Rule { path, content })
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("id が空です".to_string());
    }
    let valid = id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        return Err(format!("id には英小文字・数字・-・_・. だけ使えます: {id}"));
    }
    Ok(())
}

fn validate_color(color: &str) -> Result<(), String> {
    let bytes = color.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' || !bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        return Err(format!("color は #rrggbb の形で書いてください: {color}"));
    }
    Ok(())
}

impl Catalog {
    /// 札に書く文字。`label` の `{名前}` を置き換え、無い値は空にする。`label` が無ければ `name`。
    pub fn label_text(&self, captures: &Captures) -> String {
        let Some(template) = &self.label else {
            return self.name.clone();
        };

        let mut out = String::with_capacity(template.len());
        let mut rest = template.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after_open = &rest[open + 1..];
            if let Some(close) = after_open.find('}') {
                let key = &after_open[..close];
                if let Some(val) = captures.get(key) {
                    out.push_str(val);
                }
                rest = &after_open[close + 1..];
            } else {
                out.push('{');
                rest = after_open;
                break;
            }
        }
        out.push_str(rest);
        out
    }

    /// 札の色（RGB）。`color` が無ければ `app` の blake3 から HSL（彩度 55%、明るさ 65%）で決める。
    pub fn color_rgb(&self) -> [u8; 3] {
        if let Some(color) = &self.color {
            let r = u8::from_str_radix(&color[1..3], 16).unwrap_or(0);
            let g = u8::from_str_radix(&color[3..5], 16).unwrap_or(0);
            let b = u8::from_str_radix(&color[5..7], 16).unwrap_or(0);
            return [r, g, b];
        }
        app_color(&self.app)
    }
}

/// アプリ名から決める色（型に `color` が無いときと、1 か所の仕分けの札に使う）。
pub fn app_color(app: &str) -> [u8; 3] {
    let hash = blake3::hash(app.as_bytes());
    let bytes = hash.as_bytes();
    let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let hue = (raw as f64 / (u32::MAX as f64 + 1.0)) * 360.0;
    hsl_to_rgb(hue, 0.55, 0.65)
}

/// アプリの設定（`settings.json`）のうち、カタログに関わるもの。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CatalogSettings {
    /// カタログを読むフォルダを足す（後のものほど優先）。
    pub catalog_dirs: Vec<PathBuf>,
    /// この型の中は、重複の一覧に出さない（型の ID）。
    pub catalog_hide_duplicates: Vec<String>,
}

impl CatalogSettings {
    /// `settings.json` から読む。無い・壊れているときは空。
    pub fn from_file(path: &std::path::Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}

/// ユーザー用のカタログのフォルダ（`lisa_catalog_save` の既定の書き先）。
pub fn user_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("catalogs")
}

/// カタログを読むフォルダ（組み込みは別）。ユーザー用のフォルダ、そのあと設定の `catalog_dirs`。
/// `CatalogSet::load_with_builtin` に渡すと、組み込み → ここの順に読み、後のものほど優先する。
pub fn dirs(data_dir: &std::path::Path, settings: &CatalogSettings) -> Vec<PathBuf> {
    let mut dirs = vec![user_dir(data_dir)];
    dirs.extend(settings.catalog_dirs.iter().cloned());
    dirs
}

fn hsl_to_rgb(hue: f64, sat: f64, light: f64) -> [u8; 3] {
    let c = (1.0 - (2.0 * light - 1.0).abs()) * sat;
    let sector = hue / 60.0;
    let x = c * (1.0 - (sector % 2.0 - 1.0).abs());
    let m = light - c / 2.0;

    let (r1, g1, b1) = if sector < 1.0 {
        (c, x, 0.0)
    } else if sector < 2.0 {
        (x, c, 0.0)
    } else if sector < 3.0 {
        (0.0, c, x)
    } else if sector < 4.0 {
        (0.0, x, c)
    } else if sector < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    let to_u8 = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to_u8(r1), to_u8(g1), to_u8(b1)]
}

/// ファイルから読み込んだ 1 つのカタログ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub catalog: Catalog,
    pub source: PathBuf,
}

/// 複数のフォルダから読み込んだカタログの集まり。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogSet {
    pub catalogs: Vec<Loaded>,
    pub errors: Vec<(PathBuf, String)>,
}

impl CatalogSet {
    /// `dirs` を順に読む（各フォルダの直下の `*.toml` だけ、名前順）。
    /// 同じ `id` は後に読んだ方で置き換える。
    /// 読めないフォルダは `errors` に入れて続ける。存在しないフォルダは黙って飛ばす。
    pub fn load(dirs: &[PathBuf]) -> CatalogSet {
        let mut set = CatalogSet::default();
        set.load_dirs(dirs);
        set
    }

    /// 組み込みの型を先に読み、続いて `dirs` を順に読む。
    /// 同じ `id` は後に読んだ方で置き換える。
    pub fn load_with_builtin(dirs: &[PathBuf]) -> CatalogSet {
        let mut set = CatalogSet::default();
        for &(name, text) in builtin() {
            let source = PathBuf::from(format!("builtin:{name}"));
            set.push_parsed(source, parse(text));
        }
        set.load_dirs(dirs);
        set
    }

    fn load_dirs(&mut self, dirs: &[PathBuf]) {
        for dir in dirs {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    self.errors
                        .push((dir.clone(), format!("フォルダを読めません: {e}")));
                    continue;
                }
            };

            let mut files = Vec::new();
            let mut dir_err = None;
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("toml")
                            && !path.is_dir()
                        {
                            files.push(path);
                        }
                    }
                    Err(e) => {
                        dir_err = Some(format!("フォルダの中身を読めません: {e}"));
                        break;
                    }
                }
            }
            if let Some(msg) = dir_err {
                self.errors.push((dir.clone(), msg));
                continue;
            }

            files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

            for path in files {
                let text = match fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        self.errors
                            .push((path, format!("ファイルを読めません: {e}")));
                        continue;
                    }
                };
                self.push_parsed(path, parse(&text));
            }
        }
    }

    fn push_parsed(&mut self, source: PathBuf, result: Result<Catalog, String>) {
        match result {
            Ok(catalog) => {
                let loaded = Loaded { catalog, source };
                if let Some(existing) = self
                    .catalogs
                    .iter_mut()
                    .find(|c| c.catalog.id == loaded.catalog.id)
                {
                    *existing = loaded;
                } else {
                    self.catalogs.push(loaded);
                }
            }
            Err(e) => self.errors.push((source, e)),
        }
    }

    /// `id` が一致するカタログを返す。
    pub fn get(&self, id: &str) -> Option<&Loaded> {
        self.catalogs.iter().find(|c| c.catalog.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use regex::RegexSet;

    const UNITY_TOML: &str = r##"
id   = "unity-project"
name = "Unity プロジェクト"
app  = "Unity"
color = "#7fb4ff"
label = "Unity {ver}"

[[match]]
path = "Assets/"

[[match]]
path = "ProjectSettings/ProjectVersion.txt"
content = 're:m_EditorVersion: (?<ver>\S+)'

[[exclude]]
path = "package.json"

[[part]]
path = "Library/"
role = "cache"

[[part]]
path = "Temp/"
role = "cache"

[[part]]
path = "Assets/"
role = "data"
"##;

    struct MemTree {
        children: HashMap<i64, Option<Vec<Node>>>,
    }

    impl MemTree {
        fn new() -> Self {
            Self {
                children: HashMap::new(),
            }
        }

        fn set_children(&mut self, dir: i64, nodes: Vec<Node>) {
            self.children.insert(dir, Some(nodes));
        }

        fn set_unlisted(&mut self, dir: i64) {
            self.children.insert(dir, None);
        }
    }

    impl Tree for MemTree {
        fn children(&self, dir: i64) -> Option<Vec<Node>> {
            self.children.get(&dir).cloned().flatten()
        }
    }

    struct MemReader {
        files: HashMap<i64, Option<Vec<u8>>>,
        calls: Vec<i64>,
    }

    impl MemReader {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
                calls: Vec::new(),
            }
        }

        fn set_bytes(&mut self, id: i64, bytes: impl Into<Vec<u8>>) {
            self.files.insert(id, Some(bytes.into()));
        }

        fn set_unreadable(&mut self, id: i64) {
            self.files.insert(id, None);
        }
    }

    impl Reader for MemReader {
        fn head(&mut self, node: &Node, content: &Content) -> Option<Check> {
            self.calls.push(node.id);
            let bytes = self.files.get(&node.id)?.as_deref()?;
            Some(content.check(&node.facts, Some(bytes)))
        }
    }

    fn dir_node(id: i64, name: &str) -> Node {
        Node {
            id,
            name_nfc: name.to_string(),
            is_dir: true,
            facts: FileFacts {
                size: 0,
                hash: None,
                cloud_only: false,
            },
        }
    }

    fn file_node(id: i64, name: &str, size: u64) -> Node {
        Node {
            id,
            name_nfc: name.to_string(),
            is_dir: false,
            facts: FileFacts {
                size,
                hash: None,
                cloud_only: false,
            },
        }
    }

    #[test]
    fn parse_unity_example_and_rev() {
        let cat = parse(UNITY_TOML).unwrap();
        assert_eq!(cat.id, "unity-project");
        assert_eq!(cat.name, "Unity プロジェクト");
        assert_eq!(cat.app, "Unity");
        assert_eq!(cat.color.as_deref(), Some("#7fb4ff"));
        assert_eq!(cat.label.as_deref(), Some("Unity {ver}"));
        assert_eq!(cat.matches.len(), 2);
        assert_eq!(cat.excludes.len(), 1);
        assert_eq!(cat.parts.len(), 3);
        assert_eq!(cat.parts[0].role, Role::Cache);
        assert_eq!(cat.parts[2].role, Role::Data);
        assert_eq!(
            cat.rev,
            blake3::hash(UNITY_TOML.as_bytes()).to_hex().to_string()
        );
        assert!(cat.matches[0].path.is_dir());
        assert!(!cat.matches[1].path.is_dir());
    }

    #[test]
    fn parse_rejects_invalid_toml_and_rules() {
        // 知らないキー（トップレベル・match 内）
        assert!(
            parse("id=\"a\"\nname=\"A\"\napp=\"A\"\nextra=1\n[[match]]\npath=\"x\"\n").is_err()
        );
        assert!(parse("id=\"a\"\nname=\"A\"\napp=\"A\"\n[[match]]\npath=\"x\"\nfoo=1\n").is_err());

        // match が 0 個
        assert!(parse("id=\"a\"\nname=\"A\"\napp=\"A\"\n").is_err());

        // すべての match が ** 始まり
        assert!(
            parse("id=\"a\"\nname=\"A\"\napp=\"A\"\n[[match]]\npath=\"**/*.blend\"\n").is_err()
        );
        // 1 つでも名前で絞れる match があれば OK（exclude / part の ** 始まりも可）
        assert!(
            parse(
                "id=\"a\"\nname=\"A\"\napp=\"A\"\n\
                 [[match]]\npath=\"**/*.blend\"\n\
                 [[match]]\npath=\"Assets/\"\n\
                 [[exclude]]\npath=\"**/skip\"\n\
                 [[part]]\npath=\"**/cache/\"\nrole=\"cache\"\n"
            )
            .is_ok()
        );

        // フォルダのパターンに中身の条件
        assert!(
            parse("id=\"a\"\nname=\"A\"\napp=\"A\"\n[[match]]\npath=\"Assets/\"\nsize=\">0\"\n")
                .is_err()
        );
        assert!(
            parse(
                "id=\"a\"\nname=\"A\"\napp=\"A\"\n\
                 [[match]]\npath=\"Assets/\"\n\
                 [[exclude]]\npath=\"Temp/\"\ncontent='re:x'\n"
            )
            .is_err()
        );

        // role の打ち間違い
        assert!(
            parse(
                "id=\"a\"\nname=\"A\"\napp=\"A\"\n\
                 [[match]]\npath=\"x\"\n\
                 [[part]]\npath=\"y/\"\nrole=\"caches\"\n"
            )
            .is_err()
        );

        // id の形
        for bad_id in ["", "Unity", "a/b", "a b", "日本語"] {
            let s = format!("id=\"{bad_id}\"\nname=\"A\"\napp=\"A\"\n[[match]]\npath=\"x\"\n");
            assert!(parse(&s).is_err(), "bad_id={bad_id}");
        }
        assert!(parse("id=\"a-1_b.2\"\nname=\"A\"\napp=\"A\"\n[[match]]\npath=\"x\"\n").is_ok());

        // head の上限（1 MiB まで）
        assert!(
            parse(
                "id=\"a\"\nname=\"A\"\napp=\"A\"\n\
                 [[match]]\npath=\"x\"\ncontent='re:hi'\nhead=\"1MiB\"\n"
            )
            .is_ok()
        );
        assert!(
            parse(
                "id=\"a\"\nname=\"A\"\napp=\"A\"\n\
                 [[match]]\npath=\"x\"\ncontent='re:hi'\nhead=\"2MiB\"\n"
            )
            .is_err()
        );

        // color の形
        assert!(
            parse("id=\"a\"\nname=\"A\"\napp=\"A\"\ncolor=\"#12345\"\n[[match]]\npath=\"x\"\n")
                .is_err()
        );
    }

    #[test]
    fn glob_and_pattern_syntax() {
        // *, ?, {a,b}, \*
        let p = Pattern::parse("*.{uproject,sln}", false).unwrap();
        match p.first() {
            Segment::Name(m) => {
                assert!(m.is_match("MyGame.UPROJECT"));
                assert!(m.is_match("app.sln"));
                assert!(!m.is_match("app.txt"));
                assert_eq!(m.literal(), None);
            }
            Segment::Any => panic!("unexpected **"),
        }

        let q = Pattern::parse("v?/foo\\*bar", false).unwrap();
        assert!(matches!(q.first(), Segment::Name(m) if m.is_match("v1")));
        let q_last = match &q.segments()[1] {
            Segment::Name(m) => m,
            Segment::Any => panic!("unexpected **"),
        };
        assert!(q_last.is_match("FOO*BAR"));
        assert!(!q_last.is_match("fooxbar"));
        assert_eq!(q_last.literal(), Some("foo*bar"));

        let lit = Pattern::parse("Assets/", false).unwrap();
        assert!(matches!(lit.first(), Segment::Name(m) if m.literal() == Some("assets")));

        // **/*.blend の 0 段と 2 段
        let star = Pattern::parse("**/*.blend", false).unwrap();
        assert!(matches!(star.first(), Segment::Any));
        assert_eq!(star.first_segment_regex(), None);

        // エラーになる形
        for bad in [
            "", "/", "/abs", "a//b", ".", "a/../b", "foo/**", "**/", "**",
        ] {
            assert!(Pattern::parse(bad, false).is_err(), "bad={bad}");
        }
        assert!(Pattern::parse("{a,{b,c}}", false).is_err());
        assert!(Pattern::parse("{a,b", false).is_err());
        assert!(Pattern::parse("a\\", false).is_err());
    }

    #[test]
    fn regex_and_glob_mixed_with_regex_set() {
        let p = Pattern::parse("re:^Steam(Library)?$/*.acf", false).unwrap();
        assert!(!p.is_dir());
        assert_eq!(p.segments().len(), 2);
        match p.first() {
            Segment::Name(m) => {
                assert!(m.is_match("steamlibrary"));
                assert!(m.is_match("Steam"));
                assert!(!m.is_match("SteamApps"));
            }
            Segment::Any => panic!("unexpected **"),
        }

        // first_segment_regex で RegexSet を組み立て、大文字小文字の違いを確かめる。
        let p_ci = Pattern::parse("Assets/", false).unwrap();
        let p_cs = Pattern::parse("Temp/", true).unwrap();
        let p_re = Pattern::parse("re:^Steam(Library)?$/", false).unwrap();
        let set = RegexSet::new([
            p_ci.first_segment_regex().unwrap(),
            p_cs.first_segment_regex().unwrap(),
            p_re.first_segment_regex().unwrap(),
        ])
        .unwrap();
        assert!(set.is_match("assets"));
        assert!(!set.matches("temp").matched(1));
        assert!(set.matches("Temp").matched(1));
        assert!(set.matches("steam").matched(2));
    }

    #[test]
    fn nfd_nfc_and_case_sensitivity() {
        // NFD の名前「か + 濁点」と NFC のパターン「が」が一致する。
        let p_nfc = Pattern::parse("がっこう/", false).unwrap();
        let nfd_name = "\u{304B}\u{3099}っこう";
        assert!(matches!(p_nfc.first(), Segment::Name(m) if m.is_match(nfd_name)));

        // re: でも NFD/NFC がそろう。
        let p_re = Pattern::parse("re:^が", false).unwrap();
        assert!(matches!(p_re.first(), Segment::Name(m) if m.is_match(nfd_name)));

        // case = true では大文字小文字を区別する。
        let ci = Pattern::parse("Readme.txt", false).unwrap();
        let cs = Pattern::parse("Readme.txt", true).unwrap();
        assert!(matches!(ci.first(), Segment::Name(m) if m.is_match("README.TXT")));
        assert!(matches!(cs.first(), Segment::Name(m) if !m.is_match("README.TXT")));
        assert!(matches!(cs.first(), Segment::Name(m) if m.is_match("Readme.txt")));
    }

    #[test]
    fn dir_and_file_distinction_and_any_depth_limit() {
        let cat = parse(
            "id=\"blend\"\nname=\"Blender\"\napp=\"Blender\"\n\
             [[match]]\npath=\"Project/\"\n\
             [[match]]\npath=\"**/*.blend\"\n",
        )
        .unwrap();

        // 「Project」がファイルだとフォルダのパターンに当たらない。
        let mut tree = MemTree::new();
        tree.set_children(
            1,
            vec![file_node(2, "Project", 10), file_node(3, "a.blend", 10)],
        );
        let mut reader = MemReader::new();
        let out = evaluate(&cat, &tree, 1, &mut reader);
        assert_eq!(out.state, State::Miss);
        assert_eq!(out.failed, Some(Failed::Match(0)));

        // 0 段で a.blend が当たる。
        tree.set_children(1, vec![dir_node(2, "Project"), file_node(3, "a.blend", 10)]);
        assert_eq!(evaluate(&cat, &tree, 1, &mut reader).state, State::Match);

        // 2 段の深さ（1 -> 10 -> 11 -> 12(scene.blend)）で当たる。
        tree.set_children(1, vec![dir_node(2, "Project"), dir_node(10, "sub1")]);
        tree.set_children(10, vec![dir_node(11, "sub2")]);
        tree.set_children(11, vec![file_node(12, "scene.blend", 20)]);
        assert_eq!(evaluate(&cat, &tree, 1, &mut reader).state, State::Match);

        // ** の深さ上限 8 段までは当たり、9 段目以降は Miss 扱い。
        let mut deep_tree = MemTree::new();
        deep_tree.set_children(1, vec![dir_node(2, "Project"), dir_node(100, "d1")]);
        deep_tree.set_children(2, vec![]);
        for i in 0..8 {
            deep_tree.set_children(100 + i, vec![dir_node(101 + i, &format!("d{}", i + 2))]);
        }
        // d1..d9（9 段目 d9 の中）は上限を超えるので Miss。
        deep_tree.set_children(108, vec![file_node(200, "too_deep.blend", 10)]);
        let deep_out = evaluate(&cat, &deep_tree, 1, &mut reader);
        assert_eq!(deep_out.state, State::Miss);
        assert_eq!(deep_out.failed, Some(Failed::Match(1)));

        // d1..d8（8 段目 d8 の中）は上限以内なので Match。
        deep_tree.set_children(
            107,
            vec![dir_node(108, "d9"), file_node(201, "ok.blend", 10)],
        );
        assert_eq!(
            evaluate(&cat, &deep_tree, 1, &mut reader).state,
            State::Match
        );
    }

    #[test]
    fn evaluate_states_match_miss_exclude_pending_maybe() {
        let cat = parse(UNITY_TOML).unwrap();
        let mut tree = MemTree::new();
        tree.set_children(
            1,
            vec![dir_node(2, "Assets"), dir_node(3, "ProjectSettings")],
        );
        tree.set_children(3, vec![file_node(4, "ProjectVersion.txt", 32)]);

        let mut reader = MemReader::new();
        reader.set_bytes(4, b"m_EditorVersion: 6000.0.1f1\n");

        // 1. Match + captures + label_text
        let out = evaluate(&cat, &tree, 1, &mut reader);
        assert_eq!(out.state, State::Match);
        assert_eq!(out.failed, None);
        assert_eq!(
            out.captures.get("ver").map(String::as_str),
            Some("6000.0.1f1")
        );
        assert_eq!(cat.label_text(&out.captures), "Unity 6000.0.1f1");
        assert_eq!(cat.label_text(&Captures::new()), "Unity ");

        // 2. Maybe（Reader が読めない / クラウドのみ）
        let mut reader_none = MemReader::new();
        reader_none.set_unreadable(4);
        let out_maybe = evaluate(&cat, &tree, 1, &mut reader_none);
        assert_eq!(out_maybe.state, State::Maybe);
        assert_eq!(out_maybe.failed, None);

        // cloud_only のファイルは Reader を呼ばずに Maybe になる。
        let mut cloud_tree = MemTree::new();
        cloud_tree.set_children(
            1,
            vec![dir_node(2, "Assets"), dir_node(3, "ProjectSettings")],
        );
        let mut cloud_file = file_node(4, "ProjectVersion.txt", 32);
        cloud_file.facts.cloud_only = true;
        cloud_tree.set_children(3, vec![cloud_file]);
        let mut spy_reader = MemReader::new();
        spy_reader.set_bytes(4, b"m_EditorVersion: 6000.0.1f1\n");
        let out_cloud = evaluate(&cat, &cloud_tree, 1, &mut spy_reader);
        assert_eq!(out_cloud.state, State::Maybe);
        assert!(spy_reader.calls.is_empty());

        // 3. Pending（ProjectSettings の子が読めていない）
        let mut pending_tree = MemTree::new();
        pending_tree.set_children(
            1,
            vec![dir_node(2, "Assets"), dir_node(3, "ProjectSettings")],
        );
        pending_tree.set_unlisted(3);
        let out_pending = evaluate(&cat, &pending_tree, 1, &mut reader);
        assert_eq!(out_pending.state, State::Pending);

        // 4. exclude で Miss
        let mut exc_tree = MemTree::new();
        exc_tree.set_children(
            1,
            vec![
                dir_node(2, "Assets"),
                dir_node(3, "ProjectSettings"),
                file_node(5, "package.json", 10),
            ],
        );
        exc_tree.set_children(3, vec![file_node(4, "ProjectVersion.txt", 32)]);
        let out_exc = evaluate(&cat, &exc_tree, 1, &mut reader);
        assert_eq!(out_exc.state, State::Miss);
        assert_eq!(out_exc.failed, Some(Failed::Exclude(0)));

        // 5. match[0] が名前で外れたときは Reader を呼ばない。
        let mut miss_tree = MemTree::new();
        miss_tree.set_children(1, vec![dir_node(3, "ProjectSettings")]);
        miss_tree.set_children(3, vec![file_node(4, "ProjectVersion.txt", 32)]);
        let mut spy2 = MemReader::new();
        spy2.set_bytes(4, b"m_EditorVersion: 6000.0.1f1\n");
        let out_miss = evaluate(&cat, &miss_tree, 1, &mut spy2);
        assert_eq!(out_miss.state, State::Miss);
        assert_eq!(out_miss.failed, Some(Failed::Match(0)));
        assert!(spy2.calls.is_empty());
    }

    #[test]
    fn content_size_bytes_hash_and_capture_priority() {
        // size のいろいろな書き方と content_bytes と hash
        let data = b"\x89PNG\r\n\x1a\nhello";
        let hash = *blake3::hash(data).as_bytes();
        let hash_hex = blake3::hash(data).to_hex().to_string();

        let toml_text = format!(
            "id=\"img\"\nname=\"Img\"\napp=\"Tool\"\n\
             [[match]]\npath=\"a.png\"\ncontent_bytes=\"hex:89504e47\"\n\
             hash=\"blake3:{hash_hex}\"\nsize=\"10..20B\"\n\
             [[match]]\npath=\"meta.txt\"\ncontent='re:ver=(?<ver>\\S+) a=(?<a>\\S+)'\n\
             [[match]]\npath=\"meta2.txt\"\ncontent='re:ver=(?<ver>\\S+) b=(?<b>\\S+)'\n"
        );
        let cat = parse(&toml_text).unwrap();

        let c0 = cat.matches[0].content.as_ref().unwrap();
        assert!(c0.needs_bytes());
        assert_eq!(c0.head_len(), 64 * 1024);
        assert!(!c0.rule_key().is_empty());

        let facts_ok = FileFacts {
            size: 13,
            hash: Some(hash),
            cloud_only: false,
        };
        assert_eq!(c0.check(&facts_ok, Some(data)), Check::Hit(Captures::new()));

        // size が範囲外なら bytes 無しでも Miss
        let facts_small = FileFacts {
            size: 9,
            hash: None,
            cloud_only: false,
        };
        assert_eq!(c0.check(&facts_small, None), Check::Miss);

        // hash が未計算で size が合っていれば Unknown
        let facts_no_hash = FileFacts {
            size: 13,
            hash: None,
            cloud_only: false,
        };
        assert_eq!(c0.check(&facts_no_hash, Some(data)), Check::Unknown);

        // captures は同じ名前なら先の match を優先する。
        let mut tree = MemTree::new();
        tree.set_children(
            1,
            vec![
                Node {
                    id: 2,
                    name_nfc: "a.png".into(),
                    is_dir: false,
                    facts: facts_ok,
                },
                file_node(3, "meta.txt", 20),
                file_node(4, "meta2.txt", 20),
            ],
        );
        let mut reader = MemReader::new();
        reader.set_bytes(2, data.to_vec());
        reader.set_bytes(3, b"ver=1.0 a=first".to_vec());
        reader.set_bytes(4, b"ver=2.0 b=second".to_vec());
        let out = evaluate(&cat, &tree, 1, &mut reader);
        assert_eq!(out.state, State::Match);
        assert_eq!(out.captures.get("ver").map(String::as_str), Some("1.0"));
        assert_eq!(out.captures.get("a").map(String::as_str), Some("first"));
        assert_eq!(out.captures.get("b").map(String::as_str), Some("second"));

        // 単位と <, > の確認
        let c_lt = parse(
            "id=\"s\"\nname=\"S\"\napp=\"S\"\n\
             [[match]]\npath=\"x\"\nsize=\"<1KiB\"\n",
        )
        .unwrap();
        let sc = c_lt.matches[0].content.as_ref().unwrap();
        assert!(!sc.needs_bytes());
        assert_eq!(
            sc.check(
                &FileFacts {
                    size: 1023,
                    hash: None,
                    cloud_only: false,
                },
                None
            ),
            Check::Hit(Captures::new())
        );
        assert_eq!(
            sc.check(
                &FileFacts {
                    size: 1024,
                    hash: None,
                    cloud_only: false,
                },
                None
            ),
            Check::Miss
        );
    }

    #[test]
    fn parts_and_label_and_color() {
        let cat = parse(
            "id=\"app\"\nname=\"アプリ\"\napp=\"MyApp\"\n\
             [[match]]\npath=\"Assets/\"\n\
             [[part]]\npath=\"Assets/\"\nrole=\"data\"\n\
             [[part]]\npath=\"{Assets,Temp}/\"\nrole=\"cache\"\n\
             [[part]]\npath=\"Missing/Sub/\"\nrole=\"log\"\n",
        )
        .unwrap();

        let mut tree = MemTree::new();
        tree.set_children(
            1,
            vec![
                dir_node(2, "Assets"),
                dir_node(3, "Temp"),
                dir_node(4, "Missing"),
            ],
        );
        tree.set_unlisted(4);

        let found = parts(&cat, &tree, 1);
        // Assets は先の Data が優先され、Missing は読めていないので飛ばされる。
        assert_eq!(found, vec![(2, Role::Data), (3, Role::Cache)]);

        // label が無ければ name、color が無ければ app から決まる。
        assert_eq!(cat.label_text(&Captures::new()), "アプリ");
        let c1 = cat.color_rgb();
        let c2 = cat.color_rgb();
        assert_eq!(c1, c2);

        let unity = parse(UNITY_TOML).unwrap();
        assert_eq!(unity.color_rgb(), [0x7f, 0xb4, 0xff]);
    }

    #[test]
    fn catalog_set_loads_and_overrides_by_id() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "lisa-catalog-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let dir1 = base.join("d1");
        let dir2 = base.join("d2");
        let missing = base.join("does-not-exist");
        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        fs::write(
            dir1.join("unity.toml"),
            "id=\"unity\"\nname=\"Old\"\napp=\"Unity\"\n[[match]]\npath=\"Assets/\"\n",
        )
        .unwrap();
        fs::write(
            dir1.join("broken.toml"),
            "id=\"broken\"\nname=\"Broken\"\napp=\"B\"\n",
        )
        .unwrap();
        fs::write(dir1.join("ignore.txt"), "not toml").unwrap();
        fs::write(
            dir2.join("unity.toml"),
            "id=\"unity\"\nname=\"New\"\napp=\"Unity\"\n[[match]]\npath=\"Assets/\"\n",
        )
        .unwrap();

        let set = CatalogSet::load(&[dir1, missing, dir2]);
        assert_eq!(set.catalogs.len(), 1);
        assert_eq!(set.get("unity").unwrap().catalog.name, "New");
        assert_eq!(set.errors.len(), 1);
        assert!(set.errors[0].0.ends_with("broken.toml"));

        let _ = fs::remove_dir_all(&base);
    }
}
