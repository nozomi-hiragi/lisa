//! Lisa を MCP（Model Context Protocol）で使うためのサーバー。
//!
//! 既定では標準入出力で、1 行 1 つの JSON-RPC を話す。`--listen [ADDR]`（既定 `0.0.0.0:47800`）を
//! 付けると、LAN の他の PC から使えるように Streamable HTTP（`POST /mcp`、JSON の応答のみ）で待つ。
//!
//! - DB を読む道具（ドライブ、フォルダの中身、名前で探す、重複、同じ中身の場所、スナップショット）は、
//!   アプリが起動していなくても使える。DB は書き換えない。
//! - アプリの画面を動かす道具（`lisa_app_*`）は、起動中のアプリに `lisa_core::remote` で頼む。
//! - ユーザーのファイルやフォルダは何も動かさない（ペアリングした端末から sidecar に頼む `lisa_update` だけ、
//!   `lisa` / `lisa-mcp` の実行ファイルを入れ替える）。
//!
//! ```text
//! # 同じ PC で使う（標準入出力）
//! cargo build --release -p lisa-mcp
//! claude mcp add lisa -- /path/to/lisa/target/release/lisa-mcp
//!
//! # LAN の他の PC から使う（相手の PC で動かし、表示されたコマンドをこちらで打つ）
//! lisa-mcp --listen
//! claude mcp add --transport http lisa-<名前> http://<IP>:47800/mcp --header "Authorization: Bearer <合言葉>"
//! ```
//!
//! - `--data-dir <フォルダ>` で、DB と `control.json` と `lan.json` の場所を変えられる（試すとき用）。
//! - `--token <文字列>` で、`--listen` の合言葉を指定できる（省くと `<data_dir>/lan.json` に覚えたもの、
//!   無ければ作って保存する）。
//! - `--parent <pid>` と `--linger <秒>`（既定 600）で、親の pid が消えてから指定秒数後に自分で終わる
//!   （アプリから別プロセスとして立ち上げるとき用）。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use lisa_mcp::{
    DEFAULT_LINGER_SECS, DEFAULT_LISTEN, HttpListener, Server, SidecarInfo, load_or_create_token,
    remove_sidecar, set_custom_data_dir, set_known_app_pid, spawn_replacement_self, watch_parent,
    write_sidecar,
};
use serde_json::{Value, json};

/// 親の pid が生きているか見る間隔。
const PARENT_POLL_EVERY: Duration = Duration::from_secs(10);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut data_dir: Option<PathBuf> = None;
    let mut listen: Option<String> = None;
    let mut token: Option<String> = None;
    let mut parent: Option<u32> = None;
    let mut linger_secs: u64 = DEFAULT_LINGER_SECS;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                let Some(dir) = args.get(i + 1) else {
                    eprintln!("--data-dir にフォルダが要ります");
                    std::process::exit(1);
                };
                data_dir = Some(PathBuf::from(dir));
                set_custom_data_dir(true);
                i += 2;
            }
            "--token" => {
                let Some(t) = args.get(i + 1) else {
                    eprintln!("--token に合言葉が要ります");
                    std::process::exit(1);
                };
                token = Some(t.clone());
                i += 2;
            }
            "--listen" => {
                if let Some(next) = args.get(i + 1)
                    && !next.starts_with('-')
                {
                    listen = Some(next.clone());
                    i += 2;
                } else {
                    listen = Some(DEFAULT_LISTEN.to_string());
                    i += 1;
                }
            }
            "--parent" => {
                let Some(val) = args.get(i + 1) else {
                    eprintln!("--parent に pid が要ります");
                    std::process::exit(1);
                };
                let Ok(pid) = val.parse::<u32>() else {
                    eprintln!("--parent は数で: {val}");
                    std::process::exit(1);
                };
                parent = Some(pid);
                i += 2;
            }
            "--linger" => {
                let Some(val) = args.get(i + 1) else {
                    eprintln!("--linger に秒数が要ります");
                    std::process::exit(1);
                };
                let Ok(secs) = val.parse::<u64>() else {
                    eprintln!("--linger は数で: {val}");
                    std::process::exit(1);
                };
                linger_secs = secs;
                i += 2;
            }
            other => {
                eprintln!("知らない引数: {other}");
                std::process::exit(1);
            }
        }
    }

    let dirs = directories::ProjectDirs::from("", "", "Lisa");
    let data_dir = data_dir
        .or_else(|| {
            std::env::var_os("LISA_DATA_DIR")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| dirs.as_ref().map(|d| d.data_dir().to_path_buf()));
    let cache_dir = dirs.map(|d| d.cache_dir().to_path_buf());
    let Some(data_dir) = data_dir else {
        eprintln!("Lisa のデータの場所が分かりません");
        std::process::exit(1);
    };
    // クラウドのファイルを読んでもダウンロードしない（このプロセスはファイルの中身を読まないが、念のため）。
    let _ = lisa_core::cloud::prevent_downloads();
    let server = Server {
        data_dir,
        cache_dir,
    };

    if let Some(addr) = listen {
        run_http(server, &addr, token, parent, linger_secs);
    } else {
        run_stdio(server);
    }
}

fn run_stdio(server: Server) {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(&message),
            Err(error) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("JSON が読めません: {error}")},
            })),
        };
        if let Some(response) = response {
            let mut bytes = serde_json::to_vec(&response).expect("JSON にできる");
            bytes.push(b'\n');
            if stdout
                .write_all(&bytes)
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
}

fn run_http(
    server: Server,
    addr: &str,
    token: Option<String>,
    parent: Option<u32>,
    linger_secs: u64,
) {
    let data_dir = server.data_dir.clone();
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => match load_or_create_token(&data_dir) {
            Ok(t) => t,
            Err(error) => {
                eprintln!("合言葉を用意できませんでした: {error}");
                std::process::exit(1);
            }
        },
    };
    let name = server
        .device_label()
        .unwrap_or_else(lisa_core::volumes::device_name);
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let slug = if slug.is_empty() { "pc" } else { &slug };

    let listener = match HttpListener::bind(addr, server, token.clone()) {
        Ok(l) => l.with_sidecar(true),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let bound = listener.local_addr();
    let port = bound.port();
    let pid = std::process::id();
    let handle = listener.handle();
    if let Some(parent_pid) = parent {
        let _ = write_sidecar(&data_dir, SidecarInfo { pid, port });
        set_known_app_pid(parent_pid);
        let watch_handle = handle.clone();
        let watch_dir = data_dir.clone();
        let linger = Duration::from_secs(linger_secs);
        let _ = std::thread::Builder::new()
            .name("lisa-mcp-parent-watch".into())
            .spawn(move || {
                watch_parent(
                    parent_pid,
                    &watch_dir,
                    linger,
                    PARENT_POLL_EVERY,
                    watch_handle,
                )
            });
    }
    let host = if bound.ip().is_unspecified() {
        lisa_core::volumes::lan_address()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "<この PC のアドレス>".to_string())
    } else {
        bound.ip().to_string()
    };
    let url = format!("http://{host}:{port}/mcp");
    eprintln!("Lisa MCP を HTTP で待っています（Ctrl+C で止めます）:");
    eprintln!("  URL: {url}");
    eprintln!(
        "  claude mcp add --transport http lisa-{slug} {url} --header \"Authorization: Bearer {token}\""
    );
    listener.run();
    let restart = handle.restart_requested();
    drop(handle);
    if parent.is_some() {
        remove_sidecar(&data_dir, pid);
    }
    if restart && let Err(error) = spawn_replacement_self(&data_dir) {
        eprintln!("新しい lisa-mcp を起動できませんでした: {error}");
    }
}
