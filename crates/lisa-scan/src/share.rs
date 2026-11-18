//! ファイルを LAN の中の他の PC へ HTTP で渡す（`lisa-scan share`）。
//!
//! Mac で作った Windows 用の `lisa.exe` や `lisa-mcp.exe` を Windows の PC へ渡すためのもの。
//! 全部のアドレス（`0.0.0.0`）で待ち、`/` には落とすためのリンクと `curl` の打ち方を書いたページを、
//! `/<ファイル名>` にはファイルそのものを返す。それ以外は 404。Ctrl+C で止める。
//! 標準ライブラリだけで書いてあり、合言葉も暗号化もない。同じ LAN の誰でも落とせる。

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 8765;
/// 1 つのつなぎでの読み書きの制限時間。
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// HTTP の見出し 1 行の上限（8 KiB）。
const MAX_HEADER_LINE: usize = 8 * 1024;
/// HTTP の見出しの行数の上限。
const MAX_HEADER_LINES: usize = 100;

/// `share` の引数（`<ファイル>... [--port N | N]`）を読んで、（ファイルの一覧, ポート）にする。
/// 最後の引数が数字で、その名前のファイルが無く、前の引数が 1 つ以上あれば、それもポート番号として扱う。
pub fn parse_args(args: &[&str]) -> Result<(Vec<PathBuf>, u16), String> {
    let mut files: Vec<String> = Vec::new();
    let mut port: Option<u16> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--port" || arg == "-p" {
            let Some(val) = args.get(i + 1).filter(|v| !v.is_empty()) else {
                return Err("--port に番号が要ります".into());
            };
            let parsed: u16 = val.parse().map_err(|_| format!("ポートは数で: {val}"))?;
            if parsed == 0 {
                return Err("ポートは 1 以上で指定してください".into());
            }
            port = Some(parsed);
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--port=") {
            if val.is_empty() {
                return Err("--port に番号が要ります".into());
            }
            let parsed: u16 = val.parse().map_err(|_| format!("ポートは数で: {val}"))?;
            if parsed == 0 {
                return Err("ポートは 1 以上で指定してください".into());
            }
            port = Some(parsed);
            i += 1;
        } else if arg.starts_with('-') {
            return Err(format!("知らないオプション: {arg}"));
        } else {
            files.push(arg.to_string());
            i += 1;
        }
    }

    if port.is_none()
        && files.len() >= 2
        && let Some(last) = files.last()
        && !Path::new(last).exists()
        && let Ok(parsed) = last.parse::<u16>()
    {
        if parsed == 0 {
            return Err("ポートは 1 以上で指定してください".into());
        }
        files.pop();
        port = Some(parsed);
    }

    if files.is_empty() {
        return Err("渡すファイルを 1 つ以上指定してください".into());
    }

    let mut seen_names = HashSet::new();
    for f in &files {
        let Some(name) = Path::new(f).file_name() else {
            return Err(format!("ファイル名が分かりません: {f}"));
        };
        if !seen_names.insert(name.to_os_string()) {
            return Err(format!(
                "同じ名前のファイルは同時に渡せません: {}",
                name.to_string_lossy()
            ));
        }
    }

    let paths = files.into_iter().map(PathBuf::from).collect();
    Ok((paths, port.unwrap_or(DEFAULT_PORT)))
}

pub fn share(paths: &[PathBuf], port: u16) -> Result<(), String> {
    if paths.is_empty() {
        return Err("渡すファイルを 1 つ以上指定してください".into());
    }
    let mut items: Vec<(SharedFile, u64)> = Vec::with_capacity(paths.len());
    for p in paths {
        let path = p
            .canonicalize()
            .map_err(|e| format!("{}: {e}", p.display()))?;
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("ファイルではありません: {}", path.display()));
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("名前が UTF-8 でありません: {}", path.display()))?
            .to_string();
        let link = encode(&name);
        if items
            .iter()
            .any(|(existing, _)| existing.link == link || existing.name == name)
        {
            return Err(format!("同じ名前のファイルは同時に渡せません: {name}"));
        }
        items.push((SharedFile { path, name, link }, meta.len()));
    }

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
        .map_err(|e| format!("ポート {port} で待てませんでした: {e}"))?;
    let host = lisa_core::volumes::lan_address()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "<この PC のアドレス>".to_string());

    let summary: Vec<String> = items
        .iter()
        .map(|(f, len)| format!("{} ({})", f.name, human(*len)))
        .collect();
    println!("{} を渡しています。Ctrl+C で止めます。", summary.join("、"));
    println!();
    println!("  ブラウザで開く:   http://{host}:{port}/");
    for (f, _) in &items {
        println!(
            "  Windows で落とす: curl.exe -o {} http://{host}:{port}/{}",
            f.name, f.link
        );
    }
    println!();

    let files: Vec<SharedFile> = items.into_iter().map(|(f, _)| f).collect();
    let shared = Arc::new(files);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = Arc::clone(&shared);
        let _ = thread::Builder::new()
            .name("lisa-share-conn".into())
            .spawn(move || {
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

/// 1 つのファイルだけを `0.0.0.0:0`（空いている番号）で配り、終わったら止められる一時 HTTP サーバー。
pub struct TempFileServer {
    port: u16,
    link: String,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl TempFileServer {
    pub fn start(file_path: &Path) -> Result<Self, String> {
        let path = file_path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", file_path.display()))?;
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("ファイルではありません: {}", path.display()));
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("名前が UTF-8 でありません: {}", path.display()))?
            .to_string();
        let link = encode(&name);
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .map_err(|e| format!("空いているポートで待てませんでした: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("待ち受けの設定ができませんでした: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("待ち受けポートが分かりませんでした: {e}"))?
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let shared = Arc::new(vec![SharedFile {
            path,
            name,
            link: link.clone(),
        }]);
        let join = thread::Builder::new()
            .name("lisa-temp-share".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if stop_flag.load(Ordering::Relaxed) {
                                break;
                            }
                            let _ = stream.set_nonblocking(false);
                            let shared = Arc::clone(&shared);
                            let _ = thread::Builder::new()
                                .name("lisa-temp-share-conn".into())
                                .spawn(move || {
                                    let _ = serve(stream, &shared, Some(peer));
                                });
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })
            .map_err(|e| format!("待ち受けスレッドを作れませんでした: {e}"))?;
        Ok(Self {
            port,
            link,
            stop,
            join: Some(join),
        })
    }

    pub fn url_for(&self, host: &str) -> String {
        let host_part = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{}", self.port)
        } else {
            format!("{host}:{}", self.port)
        };
        format!("http://{host_part}/{}", self.link)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for TempFileServer {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SharedFile {
    path: PathBuf,
    name: String,
    /// URL に入れる形にしたファイル名。
    link: String,
}

fn read_bounded_line(
    reader: &mut BufReader<TcpStream>,
    max_bytes: usize,
) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let n = reader
        .take((max_bytes + 1) as u64)
        .read_until(b'\n', &mut buf)?;
    if n > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "見出しの 1 行が長すぎます",
        ));
    }
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn serve(
    stream: TcpStream,
    shared: &[SharedFile],
    peer: Option<SocketAddr>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = read_bounded_line(&mut reader, MAX_HEADER_LINE)?;
    if request.is_empty() {
        return Ok(());
    }
    // 残りの見出しは読み捨てる（空行まで、行数と 1 行の長さに上限を設ける）。
    let mut found_end = false;
    for _ in 0..MAX_HEADER_LINES {
        let line = read_bounded_line(&mut reader, MAX_HEADER_LINE)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            found_end = true;
            break;
        }
    }
    if !found_end {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "見出しの行数が多すぎます",
        ));
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
    let url_path = target.split('?').next().unwrap_or("");
    if url_path == "/" {
        let body = page(shared);
        return respond(
            &mut out,
            "200 OK",
            "text/html; charset=utf-8",
            body.as_bytes(),
            head_only,
        );
    }
    if let Some(name) = url_path.strip_prefix('/')
        && let Some(item) = shared.iter().find(|f| f.link == name)
    {
        let mut file = File::open(&item.path)?;
        let len = file.metadata()?.len();
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
             Content-Length: {len}\r\nContent-Disposition: attachment; filename=\"{}\"\r\n\
             Connection: close\r\n\r\n",
            item.link
        )?;
        if head_only {
            return Ok(());
        }
        println!("{who} が {} を落とし始めました", item.name);
        std::io::copy(&mut file, &mut out)?;
        out.flush()?;
        println!("{who} へ {} を渡し終えました", item.name);
        return Ok(());
    }
    respond(
        &mut out,
        "404 Not Found",
        "text/plain",
        b"not found",
        head_only,
    )
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

fn page(shared: &[SharedFile]) -> String {
    let mut links = String::new();
    let mut curls = String::new();
    for item in shared {
        let name = escape(&item.name);
        let link = &item.link;
        links.push_str(&format!(
            "<p><a style=\"color:#9cf;font-size:1.4em\" href=\"/{link}\" download>{name}</a></p>"
        ));
        curls.push_str(&format!(
            "<div><code>curl.exe -o {name} &lt;このページのアドレス&gt;/{link}</code></div>"
        ));
    }
    format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>Lisa</title>\
         <body style=\"font-family:sans-serif;background:#1b1b22;color:#eee;padding:2em\">\
         <h1>Lisa</h1>{links}\
         <p>ブラウザから落とすと、Windows が「PC が保護されました」と止めることがあります。\
         そのときは「詳細情報」から「実行」を押してください。PowerShell で次のように打って落とすと止められません。</p>\
         {curls}"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_share_args() {
        // 1 つのファイル（ポート省略）
        let (files, port) = parse_args(&["lisa.exe"]).unwrap();
        assert_eq!(files, vec![PathBuf::from("lisa.exe")]);
        assert_eq!(port, DEFAULT_PORT);

        // 今までの `share <ファイル> <ポート>`（最後が数字）
        let (files, port) = parse_args(&["lisa.exe", "9000"]).unwrap();
        assert_eq!(files, vec![PathBuf::from("lisa.exe")]);
        assert_eq!(port, 9000);

        // 複数ファイル + 最後が数字
        let (files, port) = parse_args(&["lisa.exe", "lisa-mcp.exe", "9000"]).unwrap();
        assert_eq!(
            files,
            vec![PathBuf::from("lisa.exe"), PathBuf::from("lisa-mcp.exe")]
        );
        assert_eq!(port, 9000);

        // 複数ファイル（ポート省略）
        let (files, port) = parse_args(&["lisa.exe", "lisa-mcp.exe"]).unwrap();
        assert_eq!(
            files,
            vec![PathBuf::from("lisa.exe"), PathBuf::from("lisa-mcp.exe")]
        );
        assert_eq!(port, DEFAULT_PORT);

        // `--port N` と `--port=N`
        let (files, port) = parse_args(&["lisa.exe", "lisa-mcp.exe", "--port", "8888"]).unwrap();
        assert_eq!(
            files,
            vec![PathBuf::from("lisa.exe"), PathBuf::from("lisa-mcp.exe")]
        );
        assert_eq!(port, 8888);

        let (files, port) = parse_args(&["--port=7777", "lisa.exe"]).unwrap();
        assert_eq!(files, vec![PathBuf::from("lisa.exe")]);
        assert_eq!(port, 7777);

        // ファイル無しや不正なオプション・同名ファイルはエラー
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["--port", "8080"]).is_err());
        assert!(parse_args(&["lisa.exe", "--port"]).is_err());
        assert!(parse_args(&["lisa.exe", "--port="]).is_err());
        assert!(parse_args(&["lisa.exe", "--port", "0"]).is_err());
        assert!(parse_args(&["lisa.exe", "--unknown"]).is_err());
        assert!(parse_args(&["dir1/lisa.exe", "dir2/lisa.exe"]).is_err());

        // HTML ページに全部のリンクとスラッシュ付きの curl 行が入る
        let html = page(&[
            SharedFile {
                path: PathBuf::from("lisa.exe"),
                name: "lisa.exe".into(),
                link: "lisa.exe".into(),
            },
            SharedFile {
                path: PathBuf::from("lisa-mcp.exe"),
                name: "lisa-mcp.exe".into(),
                link: "lisa-mcp.exe".into(),
            },
        ]);
        assert!(html.contains("href=\"/lisa.exe\""));
        assert!(html.contains("href=\"/lisa-mcp.exe\""));
        assert!(html.contains("curl.exe -o lisa.exe &lt;このページのアドレス&gt;/lisa.exe"));
        assert!(
            html.contains("curl.exe -o lisa-mcp.exe &lt;このページのアドレス&gt;/lisa-mcp.exe")
        );
    }

    #[test]
    fn temp_file_server_serves_and_stops() {
        let tmp = std::env::temp_dir().join(format!("lisa-share-test-{}", std::process::id()));
        std::fs::write(&tmp, b"hello-share-test").unwrap();
        let mut srv = TempFileServer::start(&tmp).unwrap();
        let url = srv.url_for("127.0.0.1");
        let resp = ureq::get(&url).call().unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.into_body().read_to_string().unwrap();
        assert_eq!(body, "hello-share-test");
        srv.stop();
        let _ = std::fs::remove_file(&tmp);
    }
}
