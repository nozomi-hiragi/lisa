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

pub(super) fn call(args: &Value) -> Result<Value, String> {
    if args["action"] == "processes" {
        return processes();
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
