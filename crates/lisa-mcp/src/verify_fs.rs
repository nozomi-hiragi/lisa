//! 実機検証用に、ペアリング済み端末から `lisa-verify-` で始まるフォルダの中だけを読み書きする。
//! 検証フィーチャーの debug ビルドだけに入る。

#[cfg(not(debug_assertions))]
compile_error!("verify-fs は検証用ビルドだけで使えます");

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use super::err;

const PREFIX: &str = "lisa-verify-";
const MAX_WRITE_BYTES: u64 = 4 << 30;

/// Windows で使い捨ての検証ディレクトリを確認する。
/// ドライブ直下の名前を限定し、接合点や別の場所への参照を通さない。
#[cfg(windows)]
pub fn isolated_directory(raw: &str) -> Result<PathBuf, String> {
    use std::os::windows::fs::MetadataExt;
    use std::path::Prefix;

    let path = PathBuf::from(raw);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(b'C' | b'c')))
        || components.next() != Some(Component::RootDir)
        || !matches!(components.next(), Some(Component::Normal(n)) if n.to_str().is_some_and(|n| n.starts_with(PREFIX)))
        || !raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'\\' | b'_' | b'-'))
        || raw.bytes().filter(|&b| b == b':').count() != 1
    {
        return Err("C:\\lisa-verify- で始まる検証ディレクトリを指定してください".into());
    }
    for ancestor in path.ancestors() {
        let meta = std::fs::symlink_metadata(ancestor).map_err(err)?;
        if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
            return Err("リンクや接合点は通りません".into());
        }
    }
    Ok(path)
}

#[cfg(windows)]
pub fn validate_isolated_tree(root: &Path) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;

    let mut directories = vec![root.to_path_buf()];
    while let Some(dir) = directories.pop() {
        for entry in std::fs::read_dir(dir).map_err(err)? {
            let entry = entry.map_err(err)?;
            let meta = entry.metadata().map_err(err)?;
            if meta.file_attributes() & 0x400 != 0 {
                return Err("検証ツリーにリンクや接合点が含まれています".into());
            }
            if meta.is_dir() {
                directories.push(entry.path());
            }
        }
    }
    Ok(())
}

pub(super) fn call(data_dir: &Path, args: &Value) -> Result<Value, String> {
    if args["action"] == "processes" {
        return processes();
    }
    // 端末間の中継先として最後に見つけた相手（読むだけ）。
    if args["action"] == "relay_endpoints" {
        let path = data_dir.join(lisa_core::organize::remote_keep::relay::DISCOVERY_FILE);
        return Ok(json!({
            "text": std::fs::read_to_string(path).ok(),
            "lan": super::load_lan_config(data_dir).peers.iter().map(|peer| peer.device_id.clone()).collect::<Vec<_>>(),
        }));
    }
    if args["action"] == "net" {
        return powershell_json(
            "Get-NetIPAddress -AddressFamily IPv4 | Select-Object InterfaceAlias,IPAddress,PrefixLength | ConvertTo-Json; \
             Get-NetRoute -DestinationPrefix 0.0.0.0/0 | Select-Object InterfaceAlias,NextHop,RouteMetric,InterfaceMetric | ConvertTo-Json",
        );
    }
    if args["action"] == "assoc" {
        return associations();
    }
    #[cfg(windows)]
    if args["action"] == "cpu_load" {
        if args.get("workers").is_some() {
            return external_cpu_load(args);
        }
        return cpu_load(args);
    }
    #[cfg(windows)]
    if args["action"] == "cpu_load_stop" {
        let mut load = EXTERNAL_LOAD.lock().map_err(err)?;
        if let Some(mut child) = load.take()
            && child.try_wait().map_err(err)?.is_none()
        {
            child.kill().map_err(err)?;
            child.wait().map_err(err)?;
        }
        return Ok(json!({"stopped": true}));
    }
    let path = allowed(args["path"].as_str().ok_or("path が要ります")?)?;
    match args["action"].as_str().unwrap_or("") {
        "write" => {
            let text = args["text"].as_str().ok_or("text が要ります")?;
            let repeat = args["repeat"].as_u64().unwrap_or(1);
            if text.len() as u64 * repeat > MAX_WRITE_BYTES {
                return Err("大きすぎます".into());
            }
            create_parent(&path)?;
            std::fs::write(&path, text.repeat(repeat as usize)).map_err(err)?;
            stat(&path)
        }
        "zero" => {
            let size = args["size"].as_u64().ok_or("size が要ります")?;
            if size > MAX_WRITE_BYTES {
                return Err("大きすぎます".into());
            }
            create_parent(&path)?;
            std::fs::File::create(&path)
                .and_then(|file| file.set_len(size))
                .map_err(err)?;
            stat(&path)
        }
        "stat" => stat(&path),
        "read" => {
            let bytes = std::fs::read(&path).map_err(err)?;
            if bytes.len() > 1 << 20 {
                return Err("大きすぎます".into());
            }
            Ok(json!({"text": String::from_utf8_lossy(&bytes)}))
        }
        // エクスプローラーのダブルクリックと同じ既定の動作（ShellExecute の open）で開く。
        "shell_open" => shell_open(&path),
        "tree" => {
            let mut out = Vec::new();
            walk(&path, &mut out)?;
            Ok(json!({"entries": out}))
        }
        "remove" => {
            let meta = std::fs::symlink_metadata(&path).map_err(err)?;
            if meta.is_dir() {
                std::fs::remove_dir_all(&path).map_err(err)?;
            } else {
                std::fs::remove_file(&path).map_err(err)?;
            }
            Ok(json!({"removed": true}))
        }
        "probe" => probe(&path, args["delete"] == true),
        // 書込で開けるかだけを見る（改名はしない）。予約の有無を見張るのに使う。
        "probe_write" => Ok(outcome(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map(drop),
        )),
        "probe_external" => probe_external(&path, args["delete"] == true),
        other => Err(format!("知らない action: {other}")),
    }
}

/// サイドカーのプロセス内で、計測用の一定時間だけ CPU を使う。
#[cfg(windows)]
fn cpu_load(args: &Value) -> Result<Value, String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static RUNNING: AtomicBool = AtomicBool::new(false);
    let seconds = args["seconds"]
        .as_u64()
        .filter(|n| (1..=60).contains(n))
        .ok_or("負荷をかける時間は 1〜60 秒です")?;
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(16);
    if RUNNING.swap(true, Ordering::Relaxed) {
        return Err("すでに計測用の負荷をかけています".into());
    }
    let task = std::thread::Builder::new().spawn(move || {
        let until = Instant::now() + Duration::from_secs(seconds);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    while Instant::now() < until {
                        std::hint::black_box(std::hint::black_box(1_u64).wrapping_mul(17));
                    }
                });
            }
        });
        RUNNING.store(false, Ordering::Relaxed);
    });
    if let Err(error) = task {
        RUNNING.store(false, Ordering::Relaxed);
        return Err(err(error));
    }
    Ok(json!({"workers": workers, "seconds": seconds}))
}

/// GUI やサイドカーの優先度に左右されない、通常優先度の別プロセスで負荷を作る。
#[cfg(windows)]
static EXTERNAL_LOAD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);

#[cfg(windows)]
fn external_cpu_load(args: &Value) -> Result<Value, String> {
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetPriorityClass};

    let seconds = args["seconds"]
        .as_u64()
        .filter(|n| (1..=60).contains(n))
        .ok_or("負荷をかける時間は 1〜60 秒です")?;
    let workers = args["workers"]
        .as_u64()
        .filter(|n| matches!(n, 4 | 8 | 16))
        .ok_or("負荷をかけるスレッド数は 4、8、16 です")?;
    let mut load = EXTERNAL_LOAD.lock().map_err(err)?;
    if let Some(child) = load.as_mut()
        && child.try_wait().map_err(err)?.is_none()
    {
        return Err("すでに計測用の負荷をかけています".into());
    }
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'Normal'
Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Threading;
public static class LisaVerifyLoad {
    public static void Run(int count, int seconds) {
        long end = Stopwatch.GetTimestamp() + seconds * Stopwatch.Frequency;
        var threads = new Thread[count];
        for (int i = 0; i < count; i++) {
            threads[i] = new Thread(() => {
                while (Stopwatch.GetTimestamp() < end) {}
            });
            threads[i].Start();
        }
        Console.WriteLine(Process.GetCurrentProcess().PriorityClass);
        foreach (Thread thread in threads) thread.Join();
    }
}
'@
[LisaVerifyLoad]::Run([int]$env:LISA_VERIFY_WORKERS, [int]$env:LISA_VERIFY_SECONDS)
"#;
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("SystemRoot の絶対パスが見つかりません")?;
    let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut child = Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("LISA_VERIFY_WORKERS", workers.to_string())
        .env("LISA_VERIFY_SECONDS", seconds.to_string())
        .creation_flags(0x0800_0000)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(err)?;
    let pid = child.id();
    let mut priority = String::new();
    let ready = BufReader::new(child.stdout.take().expect("標準出力")).read_line(&mut priority);
    if ready.is_err() || priority.trim() != "Normal" {
        let _ = child.kill();
        let _ = child.wait();
        return Err("通常優先度の負荷プロセスを起動できませんでした".into());
    }
    *load = Some(child);
    // SAFETY: 現在のプロセスの疑似ハンドルで、優先度を読むだけ。
    let sidecar_priority = unsafe { GetPriorityClass(GetCurrentProcess()) };
    Ok(json!({
        "workers": workers, "seconds": seconds, "pid": pid,
        "priority": priority.trim(), "sidecar_priority_class": sidecar_priority,
    }))
}

/// 絶対パスで、`..` やリンクを含まず、祖先か自分が `lisa-verify-` で始まるフォルダであるものだけ通す。
fn allowed(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err("絶対パスで、.. を含まないこと".into());
    }
    let named = path.components().any(|c| match c {
        Component::Normal(name) => name.to_string_lossy().starts_with(PREFIX),
        _ => false,
    });
    if !named {
        return Err(format!("{PREFIX} で始まるフォルダの中だけ使えます"));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if let Ok(meta) = std::fs::symlink_metadata(&current)
            && meta.file_type().is_symlink()
        {
            return Err(format!("リンクは通りません: {}", current.display()));
        }
    }
    Ok(path)
}

fn create_parent(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("親がありません")?;
    std::fs::create_dir_all(parent).map_err(err)?;
    // 作ったフォルダも PREFIX の中であることを確かめ直す。
    allowed(&parent.to_string_lossy()).map(|_| ())
}

fn stat(path: &Path) -> Result<Value, String> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(json!({"path": path, "exists": false}));
    };
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().to_string());
    Ok(json!({
        "path": path,
        "exists": true,
        "dir": meta.is_dir(),
        "size": meta.len(),
        "mtime_ns": mtime_ns,
    }))
}

fn walk(path: &Path, out: &mut Vec<Value>) -> Result<(), String> {
    out.push(stat(path)?);
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(err)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            walk(&child, out)?;
        }
    }
    Ok(())
}

fn outcome(result: std::io::Result<()>) -> Value {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => {
            json!({"ok": false, "os_error": error.raw_os_error(), "error": error.to_string()})
        }
    }
}

/// このプロセス（sidecar）から、書き込み・改名・削除ができるかを試す。
/// 改名が通ったら元に戻す。削除は `delete` のときだけ試す。
fn probe(path: &Path, delete: bool) -> Result<Value, String> {
    let is_dir = std::fs::symlink_metadata(path).map_err(err)?.is_dir();
    let write = if is_dir {
        let child = path.join("lisa-verify-probe-new.txt");
        let result = std::fs::write(&child, b"probe");
        if result.is_ok() {
            let _ = std::fs::remove_file(&child);
        }
        outcome(result)
    } else {
        outcome(
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .map(drop),
        )
    };
    let moved = path.with_file_name(format!(
        "{}.lisa-verify-moved",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let rename = std::fs::rename(path, &moved);
    if rename.is_ok() {
        std::fs::rename(&moved, path).map_err(err)?;
    }
    let mut value = json!({"process": "sidecar", "write": write, "rename": outcome(rename)});
    if delete {
        value["delete"] = outcome(if is_dir {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        });
    }
    Ok(value)
}

/// 別のプロセス（PowerShell）から同じことを試す。パスは環境変数で渡す。
fn probe_external(path: &Path, delete: bool) -> Result<Value, String> {
    if !cfg!(windows) {
        return Err("Windows だけで使えます".into());
    }
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$p = $env:LISA_VERIFY_PATH
function T($name, [scriptblock]$b) { try { & $b; "$name=ok" } catch { "$name=" + $_.Exception.Message.Trim() } }
$dir = (Get-Item -LiteralPath $p -Force).PSIsContainer
if ($dir) { T 'write' { $c = Join-Path $p 'lisa-verify-probe-new.txt'; [IO.File]::WriteAllText($c, 'probe'); Remove-Item -LiteralPath $c } }
else { T 'write' { $f = [IO.File]::Open($p, 'Open', 'Write'); $f.Close() } }
T 'rename' { $n = (Split-Path $p -Leaf) + '.lisa-verify-moved'; Rename-Item -LiteralPath $p -NewName $n; Rename-Item -LiteralPath (Join-Path (Split-Path $p) $n) -NewName (Split-Path $p -Leaf) }
if ($env:LISA_VERIFY_DELETE -eq '1') { T 'delete' { Remove-Item -LiteralPath $p -Recurse -Force } }
"#;
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("LISA_VERIFY_PATH", path)
        .env("LISA_VERIFY_DELETE", if delete { "1" } else { "0" });
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: 相手の画面に窓を出さない。
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(err)?;
    Ok(json!({
        "process": "powershell",
        "status": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

/// Lisa のプロセスのメモリ（今と最大）を返す。
fn processes() -> Result<Value, String> {
    const SCRIPT: &str = "Get-Process lisa,lisa-mcp -ErrorAction SilentlyContinue | \
        Select-Object Id,Name,WorkingSet64,PeakWorkingSet64,PrivateMemorySize64 | ConvertTo-Json";
    let mut command = std::process::Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(err)?;
    serde_json::from_slice(&output.stdout).map_err(err)
}

/// ShellExecute の既定の動作で開く（エクスプローラーのダブルクリック相当）。
fn shell_open(path: &Path) -> Result<Value, String> {
    if !cfg!(windows) {
        return Err("Windows だけで使えます".into());
    }
    const SCRIPT: &str = "Start-Process -FilePath $env:LISA_VERIFY_PATH";
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("LISA_VERIFY_PATH", path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(err)?;
    Ok(json!({
        "status": output.status.code(),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

/// 今のユーザーの .lmf・.lmd の関連付け（読むだけ）。
fn associations() -> Result<Value, String> {
    if !cfg!(windows) {
        return Err("Windows だけで使えます".into());
    }
    const SCRIPT: &str = r#"
$o = [ordered]@{}
foreach ($k in 'HKCU:\Software\Classes\.lmf','HKCU:\Software\Classes\.lmd','HKCU:\Software\Classes\.lmf\OpenWithProgids','HKCU:\Software\Classes\.lmd\OpenWithProgids','HKCU:\Software\Classes\Lisa.MergedLink\shell\open\command','HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.lmf\UserChoice','HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.lmd\UserChoice') {
  if (Test-Path $k) { $o[$k] = (Get-ItemProperty -LiteralPath $k | Select-Object * -ExcludeProperty PS* | ConvertTo-Json -Compress) } else { $o[$k] = $null }
}
$o | ConvertTo-Json
"#;
    let mut command = std::process::Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(err)?;
    serde_json::from_slice(&output.stdout).map_err(err)
}

fn powershell_json(script: &str) -> Result<Value, String> {
    let mut command = std::process::Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(err)?;
    Ok(json!({"stdout": String::from_utf8_lossy(&output.stdout)}))
}
