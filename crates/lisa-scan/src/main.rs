//! lisa-core を Bevy なしで試す CLI。
//!
//! ```text
//! cargo run -p lisa-scan -- ls <フォルダ>            1 段だけ読んで一覧を出す
//! cargo run -p lisa-scan -- tree <フォルダ> [深さ]   下までたどって数と速さを出す
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use lisa_core::listing::{self, Entry, Kind};

const USAGE: &str = "使い方:
  lisa-scan ls <フォルダ>            1 段だけ読んで一覧を出す
  lisa-scan tree <フォルダ> [深さ]   下までたどって数と速さを出す（別のボリュームには入らない）";

fn main() -> ExitCode {
    if let Err(error) = lisa_core::cloud::prevent_downloads() {
        eprintln!("クラウドのファイルをダウンロードしない設定にできませんでした: {error}");
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["ls", path] => ls(Path::new(path)),
        ["tree", path] => tree(Path::new(path), usize::MAX),
        ["tree", path, depth] => match depth.parse() {
            Ok(depth) => tree(Path::new(path), depth),
            Err(_) => Err(format!("深さは数で: {depth}")),
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
    cloud_only: u64,
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
            if entry.cloud_only {
                counts.cloud_only += 1;
            }
            match entry.kind {
                Kind::File => {
                    counts.files += 1;
                    counts.bytes += entry.size;
                    if entry.nlink.is_some_and(|n| n > 1) {
                        counts.hard_linked += 1;
                    }
                }
                Kind::Dir => {
                    counts.dirs += 1;
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
        "ファイル        {}（{}）",
        counts.files,
        human(counts.bytes)
    );
    println!("フォルダ        {}", counts.dirs);
    println!("リンク          {}", counts.symlinks);
    println!("そのほか        {}", counts.other);
    println!("クラウドのみ    {}", counts.cloud_only);
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
