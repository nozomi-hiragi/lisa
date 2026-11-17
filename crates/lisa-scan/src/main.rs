//! lisa-core を Bevy なしで試す CLI。
//!
//! ```text
//! cargo run -p lisa-scan -- ls <フォルダ>            1 段だけ読んで一覧を出す
//! cargo run -p lisa-scan -- tree <フォルダ> [深さ]   下までたどって数と速さを出す
//! cargo run -p lisa-scan -- drives                    つながっているドライブの一覧
//! cargo run -p lisa-scan -- dups [件数]               アプリの DB から重複の一覧を出す
//! cargo run -p lisa-scan -- snapshots                   アプリの DB からスナップショットの一覧を出す
//! cargo run -p lisa-scan -- share <ファイル> [ポート]  LAN の他の PC へファイルを HTTP で渡す
//! ```

mod share;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use lisa_core::listing::{self, Entry, Kind};

const USAGE: &str = "使い方:
  lisa-scan ls <フォルダ>            1 段だけ読んで一覧を出す
  lisa-scan tree <フォルダ> [深さ]   下までたどって数と速さを出す（別のボリュームには入らない）
  lisa-scan drives                    つながっているドライブの一覧
  lisa-scan dups [件数]               アプリの DB から重複の一覧を出す（空く大きさの順）
  lisa-scan snapshots                 アプリの DB からスナップショットの一覧を出す
  lisa-scan share <ファイル> [ポート] LAN の他の PC へファイルを HTTP で渡す（ポートは既定で 8765）";

fn main() -> ExitCode {
    if let Err(error) = lisa_core::cloud::prevent_downloads() {
        eprintln!("クラウドのファイルをダウンロードしない設定にできませんでした: {error}");
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["ls", path] => ls(Path::new(path)),
        ["drives"] => drives(),
        ["tree", path] => tree(Path::new(path), usize::MAX),
        ["tree", path, depth] => match depth.parse() {
            Ok(depth) => tree(Path::new(path), depth),
            Err(_) => Err(format!("深さは数で: {depth}")),
        },
        ["snapshots"] => snapshots(),
        ["share", path] => share::share(Path::new(path), share::DEFAULT_PORT),
        ["share", path, port] => match port.parse() {
            Ok(port) => share::share(Path::new(path), port),
            Err(_) => Err(format!("ポートは数で: {port}")),
        },
        ["dups"] => dups(20),
        ["dups", count] => match count.parse() {
            Ok(count) => dups(count),
            Err(_) => Err(format!("件数は数で: {count}")),
        },
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn ls(dir: &Path) -> Result<(), String> {
    let listing = listing::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in &listing.entries {
        println!("{}", describe(entry));
    }
    for (name, failure, error) in &listing.failed {
        println!("失敗  {:?}  {failure:?}: {error}", name);
    }
    println!(
        "--\n{} 件（OS のゴミ {} 件を除く、失敗 {} 件）",
        listing.entries.len(),
        listing.junk,
        listing.failed.len()
    );
    Ok(())
}

fn describe(entry: &Entry) -> String {
    let kind = match entry.kind {
        Kind::File => "file",
        Kind::Dir => "dir ",
        Kind::Symlink => "link",
        Kind::Other => "other",
    };
    let mut marks = Vec::new();
    if entry.cloud_only {
        marks.push("クラウドのみ".to_string());
    }
    if entry.other_volume {
        marks.push("別のボリューム".to_string());
    }
    if let Some(n) = entry.nlink.filter(|&n| n > 1 && entry.kind == Kind::File) {
        marks.push(format!("ハードリンク {n}"));
    }
    if let Some(target) = &entry.link_target {
        marks.push(format!("-> {}", target.display()));
    }
    if entry.name.to_str().is_none() {
        marks.push("名前が UTF-8 でない".to_string());
    }
    let size = if entry.kind == Kind::File {
        human(entry.size)
    } else {
        String::new()
    };
    format!(
        "{kind}  {size:>9}  {}  {}",
        entry.name_nfc,
        marks.join("  ")
    )
    .trim_end()
    .to_string()
}

#[derive(Default)]
struct Counts {
    files: u64,
    dirs: u64,
    symlinks: u64,
    other: u64,
    bytes: u64,
    cloud_files: u64,
    cloud_bytes: u64,
    cloud_dirs: u64,
    hard_linked: u64,
    junk: u64,
    other_volumes: u64,
    failed_dirs: u64,
    failed_entries: u64,
}

fn tree(root: &Path, max_depth: usize) -> Result<(), String> {
    let start = Instant::now();
    let mut counts = Counts::default();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut first = true;
    let mut last_report = Instant::now();
    while let Some((dir, depth)) = stack.pop() {
        let listing = match listing::read_dir(&dir) {
            Ok(listing) => listing,
            Err(error) if first => return Err(format!("{}: {error}", dir.display())),
            Err(error) => {
                counts.failed_dirs += 1;
                eprintln!(
                    "読めない  {}  {:?}: {error}",
                    dir.display(),
                    listing::Failure::of(&error)
                );
                continue;
            }
        };
        first = false;
        counts.junk += listing.junk as u64;
        counts.failed_entries += listing.failed.len() as u64;
        for entry in listing.entries {
            match entry.kind {
                Kind::File => {
                    counts.files += 1;
                    counts.bytes += entry.size;
                    if entry.cloud_only {
                        counts.cloud_files += 1;
                        counts.cloud_bytes += entry.size;
                    }
                    if entry.nlink.is_some_and(|n| n > 1) {
                        counts.hard_linked += 1;
                    }
                }
                Kind::Dir => {
                    counts.dirs += 1;
                    if entry.cloud_only {
                        counts.cloud_dirs += 1;
                    }
                    if entry.other_volume {
                        counts.other_volumes += 1;
                    } else if !entry.cloud_only && depth + 1 < max_depth {
                        stack.push((dir.join(&entry.name), depth + 1));
                    }
                }
                Kind::Symlink => counts.symlinks += 1,
                Kind::Other => counts.other += 1,
            }
        }
        if last_report.elapsed().as_secs() >= 2 {
            last_report = Instant::now();
            eprintln!(
                "… {} 件（{:.0} 秒）",
                counts.files + counts.dirs + counts.symlinks + counts.other,
                start.elapsed().as_secs_f64()
            );
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    let total = counts.files + counts.dirs + counts.symlinks + counts.other;
    println!(
        "ファイル        {}（{}。うち手元にある分 {}）",
        counts.files,
        human(counts.bytes),
        human(counts.bytes - counts.cloud_bytes)
    );
    println!("フォルダ        {}", counts.dirs);
    println!("リンク          {}", counts.symlinks);
    println!("そのほか        {}", counts.other);
    println!(
        "クラウドのみ    ファイル {}（{}）、フォルダ {}（中は数えていない）",
        counts.cloud_files,
        human(counts.cloud_bytes),
        counts.cloud_dirs
    );
    println!(
        "ハードリンク    {}（リンクが 2 つ以上あるファイル）",
        counts.hard_linked
    );
    println!("OS のゴミ       {}（数えていない）", counts.junk);
    println!("別のボリューム  {}（入っていない）", counts.other_volumes);
    println!("読めない        {}（フォルダ）", counts.failed_dirs);
    println!("情報が取れない  {}", counts.failed_entries);
    println!(
        "{total} 件を {seconds:.1} 秒（1 秒に {:.0} 件）",
        total as f64 / seconds.max(1e-9)
    );
    Ok(())
}

fn drives() -> Result<(), String> {
    let volumes = lisa_core::volumes::list().map_err(|e| e.to_string())?;
    for v in &volumes {
        let mut marks = Vec::new();
        if v.removable {
            marks.push("外付け");
        }
        if v.network {
            marks.push("ネットワーク");
        }
        if v.read_only {
            marks.push("読み取り専用");
        }
        match v.case_sensitive {
            Some(true) => marks.push("大文字小文字を区別"),
            Some(false) => {}
            None => marks.push("大文字小文字は不明"),
        }
        if v.hidden {
            marks.push("既定では読まない");
        }
        let size = match (v.capacity, v.available) {
            (Some(total), Some(free)) => format!("{} / 空き {}", human(total), human(free)),
            (Some(total), None) => human(total),
            _ => String::new(),
        };
        println!(
            "{}  [{}]  {}  {}\n    ID {}  {}",
            v.mount_path.display(),
            v.label,
            v.fs,
            size,
            v.id.as_deref().unwrap_or("（無し）"),
            marks.join("、")
        );
    }
    println!("--\n{} 個", volumes.len());
    Ok(())
}

/// アプリの DB を開く。
fn app_db() -> Result<lisa_core::db::Db, String> {
    let path = directories::ProjectDirs::from("", "", "Lisa")
        .ok_or("DB の場所が分かりません")?
        .data_dir()
        .join("lisa.db");
    if !path.exists() {
        return Err(format!("DB がありません: {}", path.display()));
    }
    lisa_core::db::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// ドライブごとのスナップショット（新しい順）。
fn snapshots() -> Result<(), String> {
    let db = app_db()?;
    let drives = lisa_core::volumes::list().map_err(|e| e.to_string())?;
    for drive in drives {
        let Some(id) = &drive.id else { continue };
        let list = db.snapshots(id).map_err(|e| e.to_string())?;
        if list.is_empty() {
            continue;
        }
        println!("{}  [{}]", drive.mount_path.display(), drive.label);
        for snap in list {
            let hash: String = snap.root_hash[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let prev = snap.prev_id.map_or("—".to_string(), |p| p.to_string());
            println!("  {}  {hash}…  前 {prev}  {} ms", snap.id, snap.taken_at);
        }
    }
    Ok(())
}

/// アプリの DB（`~/Library/Application Support/Lisa/lisa.db`）から、同じ中身のファイルのまとまりを
/// 空く大きさの順に出す。ハードリンクは同じ実体として 1 つにまとめる。
fn dups(count: usize) -> Result<(), String> {
    let path = directories::ProjectDirs::from("", "", "Lisa")
        .ok_or("DB の場所が分かりません")?
        .data_dir()
        .join("lisa.db");
    if !path.exists() {
        return Err(format!("DB がありません: {}", path.display()));
    }
    let db = lisa_core::db::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let start = Instant::now();
    let found = lisa_core::dups::groups(&db, count).map_err(|e| e.to_string())?;
    let mut total = 0;
    for group in &found {
        total += group.wasted();
        let kind = if group.kind == Kind::Dir {
            "フォルダ"
        } else {
            "ファイル"
        };
        println!(
            "{kind}  {}  {}  {} か所・合計 {}（空く {}）",
            group.name,
            human(group.size),
            group.copies.len(),
            human(group.total()),
            human(group.wasted())
        );
        for copy in &group.copies {
            println!("  - {}", copy.path.display());
            for link in &copy.hard_links {
                println!("    = {}", link.display());
            }
        }
    }
    println!(
        "--\n{} 組、合わせて {} 空く（{:.1} 秒。「=」はすぐ上と同じ実体のハードリンク）",
        found.len(),
        human(total),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

/// 読みやすい大きさ（1024 区切り）。
fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
