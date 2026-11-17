//! ファイルを 1 つ、LAN の中の他の PC へ HTTP で渡す（`lisa-scan share`）。
//!
//! Mac で作った Windows 用の `lisa.exe` を Windows の PC へ渡すためのもの。全部のアドレス
//! （`0.0.0.0`）で待ち、`/` には落とすためのリンクと `curl` の打ち方を書いたページを、
//! `/<ファイル名>` にはファイルそのものを返す。それ以外は 404。Ctrl+C で止める。
//! 標準ライブラリだけで書いてあり、合言葉も暗号化もない。同じ LAN の誰でも落とせる。

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

pub const DEFAULT_PORT: u16 = 8765;

pub fn share(path: &Path, port: u16) -> Result<(), String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("ファイルではありません: {}", path.display()));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("名前が UTF-8 でありません: {}", path.display()))?
        .to_string();

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
        .map_err(|e| format!("ポート {port} で待てませんでした: {e}"))?;
    let host = lan_address()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "<この PC のアドレス>".to_string());
    let link = encode(&name);
    println!(
        "{} ({}) を渡しています。Ctrl+C で止めます。",
        name,
        human(meta.len())
    );
    println!();
    println!("  ブラウザで開く:   http://{host}:{port}/");
    println!("  Windows で落とす: curl.exe -o {name} http://{host}:{port}/{link}");
    println!();

    let shared = Arc::new(Shared { path, name, link });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(error) = serve(stream, &shared, peer) {
                eprintln!(
                    "{}: {error}",
                    peer.map(|p| p.to_string()).unwrap_or_default()
                );
            }
        });
    }
    Ok(())
}

struct Shared {
    path: PathBuf,
    name: String,
    /// URL に入れる形にしたファイル名。
    link: String,
}

fn serve(stream: TcpStream, shared: &Shared, peer: Option<SocketAddr>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    // 残りの見出しは読み捨てる（空行まで）。
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 {
        line.clear();
    }
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let head_only = method == "HEAD";
    let mut out = stream;
    if method != "GET" && !head_only {
        return respond(
            &mut out,
            "405 Method Not Allowed",
            "text/plain",
            b"",
            head_only,
        );
    }
    let who = peer.map(|p| p.ip().to_string()).unwrap_or_default();
    match target.split('?').next().unwrap_or("") {
        "/" => {
            let page = page(shared);
            respond(
                &mut out,
                "200 OK",
                "text/html; charset=utf-8",
                page.as_bytes(),
                head_only,
            )
        }
        path if path.strip_prefix('/') == Some(shared.link.as_str()) => {
            let mut file = File::open(&shared.path)?;
            let len = file.metadata()?.len();
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                 Content-Length: {len}\r\nContent-Disposition: attachment; filename=\"{}\"\r\n\
                 Connection: close\r\n\r\n",
                shared.link
            )?;
            if head_only {
                return Ok(());
            }
            println!("{who} が落とし始めました");
            std::io::copy(&mut file, &mut out)?;
            out.flush()?;
            println!("{who} へ渡し終えました");
            Ok(())
        }
        _ => respond(
            &mut out,
            "404 Not Found",
            "text/plain",
            b"not found",
            head_only,
        ),
    }
}

fn respond(
    out: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    if !head_only {
        out.write_all(body)?;
    }
    out.flush()
}

fn page(shared: &Shared) -> String {
    let name = escape(&shared.name);
    format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>Lisa</title>\
         <body style=\"font-family:sans-serif;background:#1b1b22;color:#eee;padding:2em\">\
         <h1>Lisa</h1><p><a style=\"color:#9cf;font-size:1.4em\" href=\"/{link}\" download>{name}</a></p>\
         <p>ブラウザから落とすと、Windows が「PC が保護されました」と止めることがあります。\
         そのときは「詳細情報」から「実行」を押してください。PowerShell で \
         <code>curl.exe -o {name} &lt;このページのアドレス&gt;{link}</code> と打って落とすと止められません。</p>",
        link = shared.link,
    )
}

/// LAN で使っているこの PC のアドレス。外へ向かう UDP の口を作るだけで、何も送らない。
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// URL のパスに入れられる形にする（英数字と `-._~` 以外は %XX）。
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn human(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    format!("{mb:.1} MB")
}
