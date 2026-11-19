//! lisa-core を Bevy なしで試す CLI。
//!
//! ```text
//! cargo run -p lisa-scan -- ls <フォルダ>                       1 段だけ読んで一覧を出す
//! cargo run -p lisa-scan -- tree <フォルダ> [深さ]              下までたどって数と速さを出す
//! cargo run -p lisa-scan -- drives                              つながっているドライブの一覧
//! cargo run -p lisa-scan -- dups [件数]                         アプリの DB から重複の一覧を出す
//! cargo run -p lisa-scan -- snapshots                           アプリの DB からスナップショットの一覧を出す
//! cargo run -p lisa-scan -- catalog [--dir <フォルダ>]... [--drive <drive_id>] [--only <型の id>] [--near]
//!                                                               アプリの DB にカタログを当てて結果を出す
//! cargo run -p lisa-scan -- plan create|list|show|approve|reject|undo|purge ...
//!                                                               整理の計画を作る・見る・許可して実行する・取り消す
//! cargo run -p lisa-scan -- share <ファイル>... [--port N | N]  LAN の他の PC へファイルを HTTP で渡す
//! cargo run -p lisa-scan -- remote-logs [名前か device_id] [行数]  ペアリング済みの相手のログを読む
//! cargo run -p lisa-scan -- remote-update [名前か device_id] [--target lisa|lisa-mcp|both] [--no-restart]
//! cargo run -p lisa-scan -- remote-app [名前か device_id] start|stop|alive [--env K=V]... [--restart] [--force]
//! cargo run -p lisa-scan -- remote-screenshot [名前か device_id] [--count N] [--interval MS] [--full] [--scale N] [--out フォルダ]
//! ```

mod plan;
mod share;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use lisa_core::listing::{self, Entry, Kind};
use serde_json::{Value, json};

const USAGE: &str = "使い方:
  lisa-scan ls <フォルダ>                       1 段だけ読んで一覧を出す
  lisa-scan tree <フォルダ> [深さ]              下までたどって数と速さを出す（別のボリュームには入らない）
  lisa-scan drives                              つながっているドライブの一覧
  lisa-scan dups [件数]                         アプリの DB から重複の一覧を出す（空く大きさの順）
  lisa-scan snapshots                           アプリの DB からスナップショットの一覧を出す
  lisa-scan catalog [--dir <フォルダ>]... [--drive <drive_id>] [--only <型の id>] [--near]
                                                アプリの DB にカタログを当てて結果を出す
  lisa-scan plan create <ops.json> [--note <説明>] [--dry-run]
                                                整理の計画を作る（list / show / approve / reject / undo / purge もある）
  lisa-scan share <ファイル>... [--port N | N]  LAN の他の PC へファイルを HTTP で渡す（ポートは既定で 8765）
  lisa-scan remote-logs [名前か device_id] [行数]  ペアリング済みの相手のログ（lisa.log と crash.log）を読む
  lisa-scan remote-update [名前か device_id] [--target lisa|lisa-mcp|both] [--no-restart]
                                                ペアリング済みの相手の lisa / lisa-mcp を入れ替える（既定は both）
  lisa-scan remote-app [名前か device_id] start|stop|alive [--env K=V]... [--restart] [--force]
                                                ペアリング済みの相手のアプリを起動・終了・生死確認する
  lisa-scan remote-screenshot [名前か device_id] [--count N] [--interval MS] [--full] [--scale N] [--out フォルダ]
                                                ペアリング済みの相手の画面を連続で撮って保存する";

fn main() -> ExitCode {
    if let Err(error) = lisa_core::cloud::prevent_downloads() {
        eprintln!("クラウドのファイルをダウンロードしない設定にできませんでした: {error}");
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["ls", path] => ls(Path::new(path)),
        ["drives"] => drives(),
        ["tree", path] => tree(Path::new(path), usize::MAX),
        ["tree", path, depth] => match depth.parse() {
            Ok(depth) => tree(Path::new(path), depth),
            Err(_) => Err(format!("深さは数で: {depth}")),
        },
        ["snapshots"] => snapshots(),
        ["catalog", rest @ ..] => catalog(rest),
        ["plan", rest @ ..] => plan::run(rest),
        ["share", rest @ ..] => {
            share::parse_args(rest).and_then(|(paths, port)| share::share(&paths, port))
        }
        ["remote-logs", rest @ ..] => remote_logs(rest),
        ["remote-update", rest @ ..] => remote_update(rest),
        ["remote-app", rest @ ..] => remote_app(rest),
        ["remote-screenshot", rest @ ..] => remote_screenshot(rest),
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

/// アプリのデータのフォルダ。環境変数 `LISA_DATA_DIR` があればそちらを使う。
fn app_data_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(directories::ProjectDirs::from("", "", "Lisa")
        .ok_or("DB の場所が分かりません")?
        .data_dir()
        .to_path_buf())
}

/// アプリの DB を開く。
fn app_db() -> Result<lisa_core::db::Db, String> {
    let path = app_data_dir()?.join("lisa.db");
    if !path.exists() {
        return Err(format!("DB がありません: {}", path.display()));
    }
    lisa_core::db::Db::open(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `peers.json` から選んだ 1 台の接続先。
#[derive(Debug, Clone)]
struct SelectedPeer {
    name: String,
    addr: String,
    key: String,
    /// sidecar のポート候補（`lisa_update` / `lisa_app_start` / `lisa_app_stop` はここだけに送る）。
    sidecar_ports: Vec<u16>,
    /// 読み取り系の道具で試すポート候補（sidecar → アプリ本体 47800 の順）。
    ports: Vec<u16>,
}

/// `peers.json` を読んで、`target`（省略なら 1 台だけのときそれ）に合う相手を選ぶ。
fn select_peer(target: Option<&str>) -> Result<SelectedPeer, String> {
    let data_dir = app_data_dir()?;
    select_peer_in(&data_dir, target)
}

fn select_peer_in(data_dir: &Path, target: Option<&str>) -> Result<SelectedPeer, String> {
    let peers_path = data_dir.join("peers.json");
    let bytes = std::fs::read(&peers_path).map_err(|_| {
        format!(
            "ペアリング済みの相手がありません（{} がありません）。先にアプリでペアリングしてください。",
            peers_path.display()
        )
    })?;
    let peers: Vec<Value> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} が読めません: {e}", peers_path.display()))?;
    if peers.is_empty() {
        return Err(format!(
            "ペアリング済みの相手がありません（{} が空です）",
            peers_path.display()
        ));
    }

    let str_field = |p: &Value, k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let u16_field = |p: &Value, k: &str| {
        p.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .filter(|&n| n != 0)
    };

    let picked: &Value = match target {
        None if peers.len() == 1 => &peers[0],
        None => {
            let list: Vec<String> = peers
                .iter()
                .map(|p| format!("{} ({})", str_field(p, "name"), str_field(p, "device_id")))
                .collect();
            return Err(format!(
                "相手が複数あります。名前か device_id を指定してください: {}",
                list.join("、")
            ));
        }
        Some(q) => {
            let q_low = q.to_ascii_lowercase();
            let exact: Vec<&Value> = peers
                .iter()
                .filter(|p| {
                    str_field(p, "name").to_ascii_lowercase() == q_low
                        || str_field(p, "device_id").to_ascii_lowercase() == q_low
                })
                .collect();
            if exact.len() == 1 {
                exact[0]
            } else {
                let partial: Vec<&Value> = peers
                    .iter()
                    .filter(|p| {
                        str_field(p, "name").to_ascii_lowercase().contains(&q_low)
                            || str_field(p, "device_id")
                                .to_ascii_lowercase()
                                .contains(&q_low)
                    })
                    .collect();
                match partial.as_slice() {
                    [one] => one,
                    [] => {
                        let list: Vec<String> = peers
                            .iter()
                            .map(|p| {
                                format!("{} ({})", str_field(p, "name"), str_field(p, "device_id"))
                            })
                            .collect();
                        return Err(format!(
                            "「{q}」に合う相手がいません（覚えている相手: {}）",
                            list.join("、")
                        ));
                    }
                    _ => {
                        let list: Vec<String> = partial
                            .iter()
                            .map(|p| {
                                format!("{} ({})", str_field(p, "name"), str_field(p, "device_id"))
                            })
                            .collect();
                        return Err(format!(
                            "「{q}」に複数の相手が合います: {}",
                            list.join("、")
                        ));
                    }
                }
            }
        }
    };

    let addr = str_field(picked, "addr");
    let key = str_field(picked, "key");
    let name = str_field(picked, "name");
    if addr.is_empty() || key.is_empty() {
        return Err("相手の addr または key が peers.json にありません".into());
    }

    let fallback_port = u16_field(picked, "port").unwrap_or(47800);
    let primary_port = u16_field(picked, "mcp_port")
        .filter(|&p| p != fallback_port && p != 47800)
        .unwrap_or(47802);
    let sidecar_ports = vec![primary_port];
    let mut ports = sidecar_ports.clone();
    if !ports.contains(&fallback_port) {
        ports.push(fallback_port);
    }

    Ok(SelectedPeer {
        name,
        addr,
        key,
        sidecar_ports,
        ports,
    })
}

fn is_sidecar_only_tool(tool: &str) -> bool {
    matches!(
        tool,
        "lisa_update" | "lisa_app_start" | "lisa_app_stop" | "lisa_screenshot"
    )
}

impl SelectedPeer {
    /// 候補のポートを順に試して MCP の道具 `tool` を呼ぶ。
    /// 危ない道具（`lisa_update` / `lisa_app_start` / `lisa_app_stop`）は sidecar（47802）だけに頼み、
    /// アプリ本体の口（47800）へは落とさない。
    fn call_tool(&self, tool: &str, arguments: Value, timeout: Duration) -> Result<Value, String> {
        let candidates = if is_sidecar_only_tool(tool) {
            &self.sidecar_ports
        } else {
            &self.ports
        };
        // どのポートで何が起きたかを全部残す（最後のものだけだと、本当の理由が隠れる）。
        let mut errors: Vec<String> = Vec::new();
        for &port in candidates {
            match call_remote_tool(&self.addr, port, &self.key, tool, &arguments, timeout) {
                Ok(sc) => return Ok(sc),
                Err(e) => errors.push(format!("{}:{port}: {e}", self.addr)),
            }
        }
        let last_err = errors.join(" / ");
        if is_sidecar_only_tool(tool) {
            Err(format!(
                "{} の sidecar につながりませんでした（{last_err}）",
                self.name
            ))
        } else {
            Err(format!(
                "{} につながりませんでした（{last_err}）",
                self.name
            ))
        }
    }

    /// 相手の OS が Windows かどうかを調べる（sidecar の `initialize` の `serverInfo.os` か、`lisa_drives` のパスから）。
    /// つながらない・分からないときはエラーを返す。
    fn is_windows(&self) -> Result<bool, String> {
        let mut errors: Vec<String> = Vec::new();
        let mut init_ok = false;
        for &port in &self.sidecar_ports {
            match call_remote_method(
                &self.addr,
                port,
                &self.key,
                "initialize",
                &json!({"protocolVersion": "2025-06-18"}),
                Duration::from_secs(5),
            ) {
                Ok(res) => {
                    init_ok = true;
                    if let Some(os) = res
                        .get("serverInfo")
                        .and_then(|s| s.get("os"))
                        .and_then(Value::as_str)
                    {
                        return Ok(os.eq_ignore_ascii_case("windows"));
                    }
                    break;
                }
                Err(e) => errors.push(format!("{}:{port}: {e}", self.addr)),
            }
        }
        if !init_ok {
            return Err(format!(
                "{} の sidecar につながりませんでした（{}）",
                self.name,
                errors.join(" / ")
            ));
        }
        if let Ok(sc) = self.call_tool("lisa_drives", json!({}), Duration::from_secs(5))
            && let Some(drives) = sc.get("drives").and_then(Value::as_array)
        {
            for d in drives {
                if let Some(id) = d.get("id").and_then(Value::as_str) {
                    if id.starts_with("mac:") {
                        return Ok(false);
                    }
                    if id.starts_with("ntfs:") || id.starts_with("serial:") {
                        return Ok(true);
                    }
                }
                if let Some(m) = d.get("mount").and_then(Value::as_str) {
                    if m.starts_with('/') {
                        return Ok(false);
                    }
                    if m.contains(":\\") || m.contains(":/") {
                        return Ok(true);
                    }
                }
            }
        }
        Err(format!("{} の OS が分かりませんでした", self.name))
    }
}

/// `lisa-scan remote-logs [名前か device_id] [行数]` の引数を解釈する。
fn parse_remote_logs_args<'a>(args: &[&'a str]) -> Result<(Option<&'a str>, usize), String> {
    const DEFAULT_LINES: usize = 200;
    match args {
        [] => Ok((None, DEFAULT_LINES)),
        [one] => match one.parse::<usize>() {
            Ok(lines) => Ok((None, lines)),
            Err(_) => Ok((Some(*one), DEFAULT_LINES)),
        },
        [target, lines] => {
            let lines = lines
                .parse::<usize>()
                .map_err(|_| format!("行数は数で: {lines}"))?;
            Ok((Some(*target), lines))
        }
        _ => Err("使い方: lisa-scan remote-logs [名前か device_id] [行数]".to_string()),
    }
}

/// ペアリング済みの相手の `lisa_logs` を HTTP で呼び、`lisa.log` と `crash.log` をそのまま標準出力に出す。
fn remote_logs(args: &[&str]) -> Result<(), String> {
    let (target, lines) = parse_remote_logs_args(args)?;
    let peer = select_peer(target)?;
    let sc = peer.call_tool(
        "lisa_logs",
        json!({"lines": lines}),
        Duration::from_secs(10),
    )?;

    print_log_section("lisa.log", &sc["lisa_log"]);
    if sc["lisa_1_log"]["exists"] == json!(true) {
        print_log_section("lisa.1.log", &sc["lisa_1_log"]);
    }
    print_log_section("crash.log", &sc["crash_log"]);
    Ok(())
}

/// `remote-update` で入れ替える対象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateTarget {
    Lisa,
    LisaMcp,
    Both,
}

/// `lisa-scan remote-update [名前か device_id] [--target lisa|lisa-mcp|both] [--no-restart]` の引数を解釈する。
fn parse_remote_update_args<'a>(
    args: &[&'a str],
) -> Result<(Option<&'a str>, UpdateTarget, bool), String> {
    const USAGE_UPDATE: &str = "使い方: lisa-scan remote-update [名前か device_id] [--target lisa|lisa-mcp|both] [--no-restart]";
    let mut peer: Option<&'a str> = None;
    let mut target = UpdateTarget::Both;
    let mut restart = true;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--no-restart" {
            restart = false;
            i += 1;
        } else if arg == "--target" || arg == "-t" {
            let Some(&val) = args.get(i + 1) else {
                return Err("--target に lisa、lisa-mcp、both のどれかが要ります".into());
            };
            target = parse_update_target(val)?;
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--target=") {
            target = parse_update_target(val)?;
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("知らないオプション: {arg}\n{USAGE_UPDATE}"));
        } else if peer.is_none() {
            peer = Some(arg);
            i += 1;
        } else {
            return Err(USAGE_UPDATE.to_string());
        }
    }
    Ok((peer, target, restart))
}

fn parse_update_target(val: &str) -> Result<UpdateTarget, String> {
    match val {
        "lisa" => Ok(UpdateTarget::Lisa),
        "lisa-mcp" => Ok(UpdateTarget::LisaMcp),
        "both" => Ok(UpdateTarget::Both),
        _ => Err(format!(
            "--target は lisa、lisa-mcp、both のどれかにしてください: {val}"
        )),
    }
}

/// ペアリング済みの相手の `lisa` / `lisa-mcp` を一時の HTTP で配りながら `lisa_update` で入れ替える。
fn remote_update(args: &[&str]) -> Result<(), String> {
    let (peer_query, target, restart) = parse_remote_update_args(args)?;
    let peer = select_peer(peer_query)?;
    let is_win = peer.is_windows()?;
    let local_ip = local_ip_for(&peer.addr)
        .ok_or_else(|| "この PC の LAN アドレスが分かりませんでした".to_string())?;
    let targets: &[&str] = match target {
        UpdateTarget::Lisa => &["lisa"],
        UpdateTarget::LisaMcp => &["lisa-mcp"],
        UpdateTarget::Both => &["lisa", "lisa-mcp"],
    };

    // 先に全部のファイルの存在とハッシュを確かめる。
    let mut prepared: Vec<(&str, PathBuf, String, u64)> = Vec::new();
    for &t in targets {
        let path = built_binary_path(t, is_win);
        if !path.exists() {
            return Err(format!(
                "{} がありません。先にビルドしてください（Windows 向けは scripts/update-windows.sh）。",
                path.display()
            ));
        }
        let size = std::fs::metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        let hash =
            lisa_core::hash::hash_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let hex = lisa_core::query::hex(&hash);
        prepared.push((t, path, hex, size));
    }

    for (t, path, blake3_hex, size) in prepared {
        let mut server = share::TempFileServer::start(&path)?;
        let url = server.url_for(&local_ip.to_string());
        println!(
            "{} へ {}（{}, {}…）を渡して入れ替えています…",
            peer.name,
            path.display(),
            human(size),
            &blake3_hex[..12]
        );
        let call_res = peer.call_tool(
            "lisa_update",
            json!({
                "target": t,
                "url": url,
                "blake3": blake3_hex,
                "restart": restart,
            }),
            Duration::from_secs(300),
        );
        server.stop();
        let sc = call_res?;
        println!(
            "{} の {} を入れ替えました:\n{}",
            peer.name,
            t,
            serde_json::to_string_pretty(&sc).unwrap_or_default()
        );
    }
    Ok(())
}

fn built_binary_path(target: &str, is_windows: bool) -> PathBuf {
    if is_windows {
        PathBuf::from(format!("target/x86_64-pc-windows-gnu/release/{target}.exe"))
    } else {
        PathBuf::from(format!("target/release/{target}"))
    }
}

/// 相手のアドレス `peer_addr` へ向かうときにこの PC が使う送信元 IP。
fn local_ip_for(peer_addr: &str) -> Option<IpAddr> {
    if let Ok(ip) = peer_addr.parse::<IpAddr>() {
        if ip.is_loopback() {
            return Some(ip);
        }
        let bind_addr: SocketAddr = if ip.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
        };
        if let Ok(sock) = UdpSocket::bind(bind_addr)
            && sock.connect(SocketAddr::new(ip, 47802)).is_ok()
            && let Ok(local) = sock.local_addr()
            && !local.ip().is_unspecified()
        {
            return Some(local.ip());
        }
    }
    lisa_core::volumes::lan_address()
}

/// `remote-app` の操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppAction {
    Start,
    Stop,
    Alive,
}

/// `parse_remote_app_args` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteAppArgs<'a> {
    peer: Option<&'a str>,
    action: AppAction,
    force: bool,
    restart: bool,
    env: Vec<(String, String)>,
}

/// `lisa-scan remote-app [名前か device_id] start|stop|alive [--env K=V]... [--restart] [--force]` の引数を解釈する。
fn parse_remote_app_args<'a>(args: &[&'a str]) -> Result<RemoteAppArgs<'a>, String> {
    const USAGE_APP: &str = "使い方: lisa-scan remote-app [名前か device_id] start|stop|alive [--env K=V]... [--restart] [--force]";
    let mut peer: Option<&'a str> = None;
    let mut action: Option<AppAction> = None;
    let mut force = false;
    let mut restart = false;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--force" || arg == "-f" {
            force = true;
            i += 1;
        } else if arg == "--restart" || arg == "-r" {
            restart = true;
            i += 1;
        } else if arg == "--env" || arg == "-e" {
            let Some(&kv) = args.get(i + 1) else {
                return Err("--env に KEY=VALUE が要ります".into());
            };
            env.push(parse_env_kv(kv)?);
            i += 2;
        } else if let Some(kv) = arg.strip_prefix("--env=") {
            env.push(parse_env_kv(kv)?);
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("知らないオプション: {arg}\n{USAGE_APP}"));
        } else if action.is_none()
            && let Some(act) = match arg {
                "start" => Some(AppAction::Start),
                "stop" => Some(AppAction::Stop),
                "alive" => Some(AppAction::Alive),
                _ => None,
            }
        {
            action = Some(act);
            i += 1;
        } else if peer.is_none() && action.is_none() {
            peer = Some(arg);
            i += 1;
        } else {
            return Err(USAGE_APP.to_string());
        }
    }
    let Some(action) = action else {
        return Err(USAGE_APP.to_string());
    };
    if force && action != AppAction::Stop {
        return Err("--force は stop のときだけ使えます".into());
    }
    if (restart || !env.is_empty()) && action != AppAction::Start {
        return Err("--restart と --env は start のときだけ使えます".into());
    }
    Ok(RemoteAppArgs {
        peer,
        action,
        force,
        restart,
        env,
    })
}

fn parse_env_kv(kv: &str) -> Result<(String, String), String> {
    let Some((k, v)) = kv.split_once('=') else {
        return Err(format!("--env は KEY=VALUE の形で指定してください: {kv}"));
    };
    if k.is_empty() {
        return Err(format!("--env のキーが空です: {kv}"));
    }
    Ok((k.to_string(), v.to_string()))
}

/// ペアリング済みの相手のアプリを起動・終了・生死確認する。
fn remote_app(args: &[&str]) -> Result<(), String> {
    let parsed = parse_remote_app_args(args)?;
    let peer = select_peer(parsed.peer)?;
    let (tool, arguments, timeout) = match parsed.action {
        AppAction::Start => {
            let mut obj = serde_json::Map::new();
            if parsed.restart {
                obj.insert("restart".into(), Value::Bool(true));
            }
            if !parsed.env.is_empty() {
                let env_map: serde_json::Map<String, Value> = parsed
                    .env
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                obj.insert("env".into(), Value::Object(env_map));
            }
            let wait_secs = if parsed.restart { 25 } else { 15 };
            (
                "lisa_app_start",
                Value::Object(obj),
                Duration::from_secs(wait_secs),
            )
        }
        AppAction::Stop => (
            "lisa_app_stop",
            json!({"force": parsed.force}),
            Duration::from_secs(15),
        ),
        AppAction::Alive => ("lisa_app_alive", json!({}), Duration::from_secs(10)),
    };
    let sc = peer.call_tool(tool, arguments, timeout)?;
    println!("{}", serde_json::to_string_pretty(&sc).unwrap_or_default());
    Ok(())
}

/// `parse_remote_screenshot_args` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteScreenshotArgs<'a> {
    peer: Option<&'a str>,
    count: usize,
    interval_ms: u64,
    window: bool,
    scale: u32,
    out: Option<PathBuf>,
}

/// `lisa-scan remote-screenshot [名前か device_id] [--count N] [--interval MS] [--full] [--scale N] [--out フォルダ]` の引数を解釈する。
fn parse_remote_screenshot_args<'a>(args: &[&'a str]) -> Result<RemoteScreenshotArgs<'a>, String> {
    const USAGE_SHOT: &str = "使い方: lisa-scan remote-screenshot [名前か device_id] [--count N] [--interval MS] [--full] [--scale N] [--out フォルダ]";
    let mut peer: Option<&'a str> = None;
    let mut count: usize = 6;
    let mut interval_ms: u64 = 80;
    let mut window = true;
    let mut scale: u32 = 2;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--full" {
            window = false;
            i += 1;
        } else if arg == "--count" || arg == "-c" {
            let Some(&val) = args.get(i + 1) else {
                return Err("--count に 1〜12 の数が要ります".into());
            };
            count = parse_shot_count(val)?;
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--count=") {
            count = parse_shot_count(val)?;
            i += 1;
        } else if arg == "--interval" || arg == "--interval-ms" || arg == "-i" {
            let Some(&val) = args.get(i + 1) else {
                return Err("--interval に 30〜2000 のミリ秒が要ります".into());
            };
            interval_ms = parse_shot_interval(val)?;
            i += 2;
        } else if let Some(val) = arg
            .strip_prefix("--interval=")
            .or_else(|| arg.strip_prefix("--interval-ms="))
        {
            interval_ms = parse_shot_interval(val)?;
            i += 1;
        } else if arg == "--scale" || arg == "-s" {
            let Some(&val) = args.get(i + 1) else {
                return Err("--scale に 1〜4 の数が要ります".into());
            };
            scale = parse_shot_scale(val)?;
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--scale=") {
            scale = parse_shot_scale(val)?;
            i += 1;
        } else if arg == "--out" || arg == "-o" {
            let Some(&val) = args.get(i + 1) else {
                return Err("--out に出力先のフォルダが要ります".into());
            };
            if val.is_empty() {
                return Err("--out のフォルダが空です".into());
            }
            out = Some(PathBuf::from(val));
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--out=") {
            if val.is_empty() {
                return Err("--out のフォルダが空です".into());
            }
            out = Some(PathBuf::from(val));
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("知らないオプション: {arg}\n{USAGE_SHOT}"));
        } else if peer.is_none() {
            peer = Some(arg);
            i += 1;
        } else {
            return Err(USAGE_SHOT.to_string());
        }
    }
    Ok(RemoteScreenshotArgs {
        peer,
        count,
        interval_ms,
        window,
        scale,
        out,
    })
}

fn parse_shot_count(val: &str) -> Result<usize, String> {
    let n: usize = val
        .parse()
        .map_err(|_| format!("--count は 1〜12 の整数で指定してください: {val}"))?;
    if !(1..=12).contains(&n) {
        return Err(format!("--count は 1〜12 の範囲で指定してください: {n}"));
    }
    Ok(n)
}

fn parse_shot_interval(val: &str) -> Result<u64, String> {
    let n: u64 = val
        .parse()
        .map_err(|_| format!("--interval は 30〜2000 の整数で指定してください: {val}"))?;
    if !(30..=2000).contains(&n) {
        return Err(format!(
            "--interval は 30〜2000 の範囲で指定してください: {n}"
        ));
    }
    Ok(n)
}

fn parse_shot_scale(val: &str) -> Result<u32, String> {
    let n: u32 = val
        .parse()
        .map_err(|_| format!("--scale は 1〜4 の整数で指定してください: {val}"))?;
    if !(1..=4).contains(&n) {
        return Err(format!("--scale は 1〜4 の範囲で指定してください: {n}"));
    }
    Ok(n)
}

/// 既定の出力先 `/tmp/lisa-shots/<時刻>/` を作る。
fn default_shots_dir() -> PathBuf {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    PathBuf::from("/tmp/lisa-shots").join(format_timestamp_for_dir(now_ms))
}

fn format_timestamp_for_dir(epoch_ms: u128) -> String {
    let secs = (epoch_ms / 1000) as i64;
    let millis = (epoch_ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let hour = sod / 3600;
    let min = (sod % 3600) / 60;
    let sec = sod % 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}{m:02}{d:02}-{hour:02}{min:02}{sec:02}-{millis:03}")
}

/// 標準の base64 文字列をバイト列に戻す。
fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !cleaned.len().is_multiple_of(4) {
        return Err("base64 の長さが 4 の倍数ではありません".into());
    }
    let val = |b: u8| -> Result<u32, String> {
        match b {
            b'A'..=b'Z' => Ok(u32::from(b - b'A')),
            b'a'..=b'z' => Ok(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(b - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("base64 に使えない文字があります: {}", b as char)),
        }
    };
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    let chunks = cleaned.as_chunks::<4>().0;
    for (idx, chunk) in chunks.iter().enumerate() {
        let is_last = idx + 1 == cleaned.len() / 4;
        if (chunk[0] == b'=' || chunk[1] == b'=')
            || (!is_last && (chunk[2] == b'=' || chunk[3] == b'='))
            || (chunk[2] == b'=' && chunk[3] != b'=')
        {
            return Err("base64 のパディングが不正です".into());
        }
        let v0 = val(chunk[0])?;
        let v1 = val(chunk[1])?;
        let v2 = if chunk[2] == b'=' { 0 } else { val(chunk[2])? };
        let v3 = if chunk[3] == b'=' { 0 } else { val(chunk[3])? };
        let n = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push(((n >> 16) & 0xff) as u8);
        if chunk[2] != b'=' {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if chunk[3] != b'=' {
            out.push((n & 0xff) as u8);
        }
    }
    Ok(out)
}

/// 保存した 1 枚の情報。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SavedShot {
    bmp_path: PathBuf,
    png_path: Option<PathBuf>,
    taken_at: i64,
    elapsed_ms: u64,
}

/// `lisa_screenshot` の結果 `sc` を `out_dir` に BMP（と Mac なら `sips` で PNG）として保存する。
fn save_screenshots_to_dir(sc: &Value, out_dir: &Path) -> Result<Vec<SavedShot>, String> {
    let shots = sc
        .get("shots")
        .and_then(Value::as_array)
        .ok_or_else(|| "返事に shots がありません".to_string())?;
    if shots.is_empty() {
        return Err("撮れた画面が 0 枚でした".into());
    }
    std::fs::create_dir_all(out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let mut saved = Vec::with_capacity(shots.len());
    for (idx, shot) in shots.iter().enumerate() {
        let b64 = shot
            .get("bmp_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{} 枚目に bmp_base64 がありません", idx + 1))?;
        let bmp_bytes = base64_decode(b64)?;
        let bmp_path = out_dir.join(format!("shot-{:02}.bmp", idx + 1));
        std::fs::write(&bmp_path, &bmp_bytes)
            .map_err(|e| format!("{}: {e}", bmp_path.display()))?;
        let png_path = convert_bmp_to_png_if_macos(&bmp_path);
        let taken_at = shot.get("taken_at").and_then(Value::as_i64).unwrap_or(0);
        let elapsed_ms = shot.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0);
        saved.push(SavedShot {
            bmp_path,
            png_path,
            taken_at,
            elapsed_ms,
        });
    }
    Ok(saved)
}

fn convert_bmp_to_png_if_macos(bmp_path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let png_path = bmp_path.with_extension("png");
        let ok = std::process::Command::new("sips")
            .args(["-s", "format", "png"])
            .arg(bmp_path)
            .arg("--out")
            .arg(&png_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok && png_path.exists() {
            return Some(png_path);
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bmp_path;
        None
    }
}

/// ペアリング済みの相手の `lisa_screenshot` を呼び、BMP（と Mac なら PNG）を保存する。
fn remote_screenshot(args: &[&str]) -> Result<(), String> {
    let parsed = parse_remote_screenshot_args(args)?;
    let peer = select_peer(parsed.peer)?;
    let out_dir = parsed.out.unwrap_or_else(default_shots_dir);
    let wait_ms = (parsed.count as u64)
        .saturating_mul(parsed.interval_ms)
        .saturating_add(30_000);
    let sc = peer.call_tool(
        "lisa_screenshot",
        json!({
            "count": parsed.count,
            "interval_ms": parsed.interval_ms,
            "window": parsed.window,
            "scale": parsed.scale,
        }),
        Duration::from_millis(wait_ms),
    )?;
    let saved = save_screenshots_to_dir(&sc, &out_dir)?;
    let width = sc.get("width").and_then(Value::as_u64).unwrap_or(0);
    let height = sc.get("height").and_then(Value::as_u64).unwrap_or(0);
    let window = sc.get("window").and_then(Value::as_bool).unwrap_or(false);
    let area_label = if window {
        "Lisa の窓"
    } else {
        "画面全体"
    };
    println!(
        "{} の画面（{}、{}x{}、{} 枚）を保存しました: {}",
        peer.name,
        area_label,
        width,
        height,
        saved.len(),
        out_dir.display()
    );
    if let Some(note) = sc.get("note").and_then(Value::as_str) {
        println!("  注: {note}");
    }
    for s in &saved {
        match &s.png_path {
            Some(png) => println!(
                "  {} / {}  ({} ms, +{} ms)",
                s.bmp_path.display(),
                png.display(),
                s.taken_at,
                s.elapsed_ms
            ),
            None => println!(
                "  {}  ({} ms, +{} ms)",
                s.bmp_path.display(),
                s.taken_at,
                s.elapsed_ms
            ),
        }
    }
    Ok(())
}

fn call_remote_method(
    addr: &str,
    port: u16,
    key: &str,
    method: &str,
    params: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let host = if addr.contains(':') && !addr.starts_with('[') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    };
    let url = format!("http://{host}/mcp");
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let mut response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {key}"))
        .send(&body)
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if status == 401 {
        return Err("認証が通りませんでした（401）".into());
    }
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut response.body_mut().as_reader(), &mut text)
        .map_err(|e| e.to_string())?;
    let reply: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if let Some(err) = reply.get("error") {
        return Err(err.to_string());
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| "result がありません".to_string())
}

fn call_remote_tool(
    addr: &str,
    port: u16,
    key: &str,
    tool: &str,
    arguments: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let result = call_remote_method(
        addr,
        port,
        key,
        "tools/call",
        &json!({
            "name": tool,
            "arguments": arguments,
        }),
        timeout,
    )?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let msg = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("エラー");
        return Err(msg.to_string());
    }
    result
        .get("structuredContent")
        .cloned()
        .ok_or_else(|| "structuredContent がありません".to_string())
}

fn print_log_section(label: &str, entry: &Value) {
    let path = entry.get("path").and_then(Value::as_str).unwrap_or(label);
    if entry.get("exists").and_then(Value::as_bool) != Some(true) {
        println!("=== {label} ({path}: ありません) ===");
        return;
    }
    let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
    let total = entry
        .get("total_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("=== {label} ({path}, {size} B, {total} 行) ===");
    if let Some(err) = entry.get("error").and_then(Value::as_str) {
        println!("（{err}）");
        return;
    }
    if let Some(text) = entry.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        println!("{text}");
    }
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

/// アプリの DB から、同じ中身のファイルのまとまりを空く大きさの順に出す。
/// ハードリンクは同じ実体として 1 つにまとめる。
fn dups(count: usize) -> Result<(), String> {
    let db = app_db()?;
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

/// `lisa-scan catalog` の引数。
#[derive(Debug, Default, PartialEq, Eq)]
struct CatalogArgs {
    dirs: Vec<PathBuf>,
    drive: Option<String>,
    only: Option<String>,
    near: bool,
}

fn parse_catalog_args(args: &[&str]) -> Result<CatalogArgs, String> {
    const USAGE_CATALOG: &str = "使い方: lisa-scan catalog [--dir <フォルダ>]... [--drive <drive_id>] [--only <型の id>] [--near]";
    let mut parsed = CatalogArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--near" {
            parsed.near = true;
            i += 1;
        } else if arg == "--dir" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--dir にフォルダが要ります\n{USAGE_CATALOG}"));
            };
            if val.is_empty() {
                return Err("--dir が空です".into());
            }
            parsed.dirs.push(PathBuf::from(val));
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--dir=") {
            if val.is_empty() {
                return Err("--dir が空です".into());
            }
            parsed.dirs.push(PathBuf::from(val));
            i += 1;
        } else if arg == "--drive" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--drive に drive_id が要ります\n{USAGE_CATALOG}"));
            };
            set_single_opt(&mut parsed.drive, "--drive", val)?;
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--drive=") {
            set_single_opt(&mut parsed.drive, "--drive", val)?;
            i += 1;
        } else if arg == "--only" {
            let Some(&val) = args.get(i + 1) else {
                return Err(format!("--only に型の id が要ります\n{USAGE_CATALOG}"));
            };
            set_single_opt(&mut parsed.only, "--only", val)?;
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--only=") {
            set_single_opt(&mut parsed.only, "--only", val)?;
            i += 1;
        } else {
            return Err(format!("知らない引数: {arg}\n{USAGE_CATALOG}"));
        }
    }
    Ok(parsed)
}

fn set_single_opt(slot: &mut Option<String>, name: &str, val: &str) -> Result<(), String> {
    if val.is_empty() {
        return Err(format!("{name} が空です"));
    }
    if slot.is_some() {
        return Err(format!("{name} は 1 回だけ指定してください"));
    }
    *slot = Some(val.to_string());
    Ok(())
}

/// `settings.json` の場所（アプリと同じ。`LISA_DATA_DIR` があればその中）。
fn settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    directories::ProjectDirs::from("", "", "Lisa").map(|d| d.config_dir().join("settings.json"))
}

fn state_label(state: lisa_core::catalog::State) -> &'static str {
    match state {
        lisa_core::catalog::State::Match => "match",
        lisa_core::catalog::State::Maybe => "maybe",
        lisa_core::catalog::State::Pending => "pending",
        lisa_core::catalog::State::Miss => "miss",
    }
}

/// アプリの DB にカタログを当てて結果を出す（DB には書かない）。
fn catalog(args: &[&str]) -> Result<(), String> {
    let parsed = parse_catalog_args(args)?;

    let settings = settings_path()
        .map(|p| lisa_core::catalog::CatalogSettings::from_file(&p))
        .unwrap_or_default();
    let mut dirs = lisa_core::catalog::dirs(&app_data_dir()?, &settings);
    dirs.extend(parsed.dirs);

    let mut set = lisa_core::catalog::CatalogSet::load_with_builtin(&dirs);
    for (path, reason) in &set.errors {
        println!("読めないカタログ  {}: {reason}", path.display());
    }
    if let Some(only_id) = &parsed.only {
        set.catalogs.retain(|l| l.catalog.id == *only_id);
        if set.catalogs.is_empty() {
            return Err(format!("カタログが見つかりません: {only_id}"));
        }
    }

    let db = app_db()?;
    let start = Instant::now();
    let (found, progress) =
        lisa_core::catalog::db::find(&db, &set, parsed.drive.as_deref(), false, &|| true)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "途中で中断されました".to_string())?;

    for loaded in &set.catalogs {
        let cat = &loaded.catalog;
        let items: Vec<_> = found.iter().filter(|f| f.catalog_id == cat.id).collect();
        if items.is_empty() && parsed.only.is_none() {
            continue;
        }
        println!("{}（{}）  {} か所", cat.name, cat.id, items.len());
        for item in items {
            let path = lisa_core::query::path_of(&db, item.entry_id)
                .ok()
                .flatten()
                .map_or_else(
                    || format!("entry#{}", item.entry_id),
                    |l| l.path.display().to_string(),
                );
            let size = lisa_core::query::folder_total(&db, item.entry_id)
                .map(|(_, _, bytes)| human(bytes))
                .unwrap_or_else(|_| "-".to_string());
            let label = cat.label_text(&item.captures);
            println!("  {:<7}  {label}  {path}  {size}", state_label(item.state));
        }
    }

    if parsed.near {
        let nears = lisa_core::catalog::db::find_near_misses(
            &db,
            &set,
            parsed.drive.as_deref(),
            false,
            &|| true,
        )
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
        if !nears.is_empty() {
            println!("惜しい候補（外れた match が 1 つだけ）:");
            for n in &nears {
                let path = lisa_core::query::path_of(&db, n.entry_id)
                    .ok()
                    .flatten()
                    .map_or_else(
                        || format!("entry#{}", n.entry_id),
                        |l| l.path.display().to_string(),
                    );
                let pat = set
                    .get(&n.catalog_id)
                    .and_then(|l| l.catalog.matches.get(n.failed_match))
                    .map(|r| lisa_core::catalog::db::pattern_text(&r.path))
                    .unwrap_or_else(|| format!("match #{}", n.failed_match));
                println!(
                    "  {}  {path}  (外れた match #{}: {pat})",
                    n.catalog_id,
                    n.failed_match + 1
                );
            }
        }
    }

    println!(
        "--\n{} か所（候補 {}、確かめたフォルダ {}、読んだ {}、{:.2} 秒）",
        found.len(),
        progress.candidates,
        progress.folders_checked,
        human(progress.bytes_read),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_logs_arguments() {
        assert_eq!(parse_remote_logs_args(&[]).unwrap(), (None, 200));
        assert_eq!(parse_remote_logs_args(&["500"]).unwrap(), (None, 500));
        assert_eq!(
            parse_remote_logs_args(&["win-pc"]).unwrap(),
            (Some("win-pc"), 200)
        );
        assert_eq!(
            parse_remote_logs_args(&["win-pc", "50"]).unwrap(),
            (Some("win-pc"), 50)
        );
        assert!(parse_remote_logs_args(&["win-pc", "not-a-number"]).is_err());
        assert!(parse_remote_logs_args(&["a", "b", "c"]).is_err());
    }

    #[test]
    fn parse_remote_update_and_app_arguments() {
        assert_eq!(
            parse_remote_update_args(&[]).unwrap(),
            (None, UpdateTarget::Both, true)
        );
        assert_eq!(
            parse_remote_update_args(&["win-pc", "--target", "lisa", "--no-restart"]).unwrap(),
            (Some("win-pc"), UpdateTarget::Lisa, false)
        );
        assert_eq!(
            parse_remote_update_args(&["--target=lisa-mcp"]).unwrap(),
            (None, UpdateTarget::LisaMcp, true)
        );
        assert!(parse_remote_update_args(&["--target", "other"]).is_err());

        assert_eq!(
            parse_remote_app_args(&["start"]).unwrap(),
            RemoteAppArgs {
                peer: None,
                action: AppAction::Start,
                force: false,
                restart: false,
                env: vec![],
            }
        );
        assert_eq!(
            parse_remote_app_args(&[
                "win-pc",
                "start",
                "--restart",
                "--env",
                "LISA_UPDATE_MODE=continuous",
                "--env=LISA_PRESENT_MODE=fifo",
            ])
            .unwrap(),
            RemoteAppArgs {
                peer: Some("win-pc"),
                action: AppAction::Start,
                force: false,
                restart: true,
                env: vec![
                    ("LISA_UPDATE_MODE".into(), "continuous".into()),
                    ("LISA_PRESENT_MODE".into(), "fifo".into()),
                ],
            }
        );
        assert_eq!(
            parse_remote_app_args(&["win-pc", "stop", "--force"]).unwrap(),
            RemoteAppArgs {
                peer: Some("win-pc"),
                action: AppAction::Stop,
                force: true,
                restart: false,
                env: vec![],
            }
        );
        assert_eq!(
            parse_remote_app_args(&["alive"]).unwrap(),
            RemoteAppArgs {
                peer: None,
                action: AppAction::Alive,
                force: false,
                restart: false,
                env: vec![],
            }
        );
        assert!(parse_remote_app_args(&["start", "--force"]).is_err());
        assert!(parse_remote_app_args(&["stop", "--restart"]).is_err());
        assert!(parse_remote_app_args(&["stop", "--env", "WGPU_BACKEND=dx12"]).is_err());
        assert!(parse_remote_app_args(&["start", "--env", "NO_EQUALS"]).is_err());
        assert!(parse_remote_app_args(&["win-pc"]).is_err());
    }

    #[test]
    fn select_peer_ports_and_is_windows_error() {
        let dir = std::env::temp_dir().join(format!("lisa-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 明示された port / mcp_port があるときは、47802 / 47800 を勝手に候補へ足さない。
        std::fs::write(
            dir.join("peers.json"),
            r#"[{"device_id":"dev-a","name":"A","key":"k","addr":"127.0.0.1","port":47853,"mcp_port":47852}]"#,
        )
        .unwrap();
        let peer = select_peer_in(&dir, Some("A")).unwrap();
        assert_eq!(peer.sidecar_ports, vec![47852]);
        assert_eq!(peer.ports, vec![47852, 47853]);

        // 省略されたときだけ既定の 47802 / 47800 を使う。
        std::fs::write(
            dir.join("peers.json"),
            r#"[{"device_id":"dev-b","name":"B","key":"k","addr":"127.0.0.1"}]"#,
        )
        .unwrap();
        let peer_default = select_peer_in(&dir, None).unwrap();
        assert_eq!(peer_default.sidecar_ports, vec![47802]);
        assert_eq!(peer_default.ports, vec![47802, 47800]);

        // つながらないポートに対して is_windows を呼ぶと、Windows と決めつけずに Err を返す。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = listener.local_addr().unwrap().port();
        drop(listener);
        let unreachable = SelectedPeer {
            name: "dead".into(),
            addr: "127.0.0.1".into(),
            key: "k".into(),
            sidecar_ports: vec![closed_port],
            ports: vec![closed_port],
        };
        assert!(unreachable.is_windows().is_err());

        // local_ip_for は 127.0.0.1 なら 127.0.0.1 を返し、IPv4 アドレスなら有効な送信元 IP を返す。
        assert_eq!(
            local_ip_for("127.0.0.1"),
            Some("127.0.0.1".parse().unwrap())
        );
        assert!(local_ip_for("192.0.2.1").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_remote_screenshot_and_save_bmp_png() {
        assert_eq!(
            parse_remote_screenshot_args(&[]).unwrap(),
            RemoteScreenshotArgs {
                peer: None,
                count: 6,
                interval_ms: 80,
                window: true,
                scale: 2,
                out: None,
            }
        );
        assert_eq!(
            parse_remote_screenshot_args(&[
                "win-pc",
                "--count",
                "4",
                "--interval",
                "120",
                "--full",
                "--scale",
                "1",
                "--out",
                "/tmp/custom-shots",
            ])
            .unwrap(),
            RemoteScreenshotArgs {
                peer: Some("win-pc"),
                count: 4,
                interval_ms: 120,
                window: false,
                scale: 1,
                out: Some(PathBuf::from("/tmp/custom-shots")),
            }
        );
        assert_eq!(
            parse_remote_screenshot_args(&[
                "--count=12",
                "--interval=2000",
                "--scale=4",
                "--out=/tmp/s",
                "win-pc",
            ])
            .unwrap(),
            RemoteScreenshotArgs {
                peer: Some("win-pc"),
                count: 12,
                interval_ms: 2000,
                window: true,
                scale: 4,
                out: Some(PathBuf::from("/tmp/s")),
            }
        );
        for bad in [
            &["--count", "0"][..],
            &["--count", "13"][..],
            &["--interval", "29"][..],
            &["--interval", "2001"][..],
            &["--scale", "0"][..],
            &["--scale", "5"][..],
            &["--out", ""][..],
            &["--unknown"][..],
            &["win-a", "win-b"][..],
        ] {
            assert!(
                parse_remote_screenshot_args(bad).is_err(),
                "args {bad:?} should be rejected"
            );
        }

        // 1x1 の有効な 24 ビット BMP（54 + 4 = 58 バイト）を base64 にして保存を試す
        let mut bmp = Vec::with_capacity(58);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&58u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&1i32.to_le_bytes());
        bmp.extend_from_slice(&1i32.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&4u32.to_le_bytes());
        bmp.extend_from_slice(&2835i32.to_le_bytes());
        bmp.extend_from_slice(&2835i32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&[255, 128, 64, 0]);
        // base64("BM:..." 58 bytes):
        let b64 =
            "Qk06AAAAAAAAADYAAAAoAAAAAQAAAAEAAAABABgAAAAAAAQAAAATCwAAEwsAAAAAAAAAAAAA/4BAAA==";
        assert_eq!(base64_decode(b64).unwrap(), bmp);

        let out_dir =
            std::env::temp_dir().join(format!("lisa-scan-shot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out_dir);
        let sc = json!({
            "count": 1,
            "width": 1,
            "height": 1,
            "window": true,
            "shots": [
                {
                    "index": 0,
                    "taken_at": 1700000000000i64,
                    "elapsed_ms": 0,
                    "width": 1,
                    "height": 1,
                    "bytes": 58,
                    "bmp_base64": b64,
                }
            ]
        });
        let saved = save_screenshots_to_dir(&sc, &out_dir).unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].bmp_path.exists());
        assert_eq!(std::fs::read(&saved[0].bmp_path).unwrap(), bmp);
        #[cfg(target_os = "macos")]
        {
            let png = saved[0].png_path.as_ref().expect("sips should create PNG");
            assert!(png.exists());
            let png_bytes = std::fs::read(png).unwrap();
            assert_eq!(&png_bytes[0..8], b"\x89PNG\r\n\x1a\n");
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn parse_catalog_arguments() {
        assert_eq!(parse_catalog_args(&[]).unwrap(), CatalogArgs::default());
        assert_eq!(
            parse_catalog_args(&[
                "--dir",
                "/tmp/c1",
                "--dir=/tmp/c2",
                "--drive",
                "mac:1",
                "--only=unity-project",
                "--near",
            ])
            .unwrap(),
            CatalogArgs {
                dirs: vec![PathBuf::from("/tmp/c1"), PathBuf::from("/tmp/c2")],
                drive: Some("mac:1".into()),
                only: Some("unity-project".into()),
                near: true,
            }
        );
        for bad in [
            &["--dir"][..],
            &["--dir="][..],
            &["--drive"][..],
            &["--drive", "a", "--drive", "b"][..],
            &["--only"][..],
            &["--unknown"][..],
            &["extra"][..],
        ] {
            assert!(parse_catalog_args(bad).is_err(), "args {bad:?} should fail");
        }
    }
}
