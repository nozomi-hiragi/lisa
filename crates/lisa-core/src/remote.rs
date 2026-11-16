//! 起動中のアプリを外から（`lisa-mcp` など）動かすための口。
//!
//! - アプリは `127.0.0.1` の空いている番号で待ち、番号と合言葉（起動ごとに作る）を `control.json`
//!   （DB と同じフォルダ）に書く。終わるときに消す。
//! - 1 つのつなぎで、1 行の JSON（`Request`）を送ると 1 行の JSON（`Reply`）が返る。何往復してもよい。
//! - 合言葉が違えば何もしない（同じ PC のほかのユーザーやブラウザから動かされないように）。
//! - 頼みはアプリの画面を動かすだけで、ファイルは動かさない。

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `control.json` の名前。
pub const FILE_NAME: &str = "control.json";

/// アプリの待っている場所。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

/// アプリへの頼み。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// 今の様子を聞く。
    State,
    /// DB の道のり（ドライブの一番上からの `entry.id`）の場所を画面に出す。
    Show { chain: Vec<i64> },
    /// 重複の一覧を開け閉めする。`select` はその組のハッシュ（16 進）。一覧ができるまで選べない。
    Dups {
        open: bool,
        #[serde(default)]
        select: Option<String>,
    },
    /// フォルダを先に読み直して（ハッシュして）もらう。
    Rescan { path: PathBuf },
    /// 裏の読み込みを一時停止する・再開する。
    Pause { paused: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    #[serde(flatten)]
    pub command: Command,
}

/// アプリの返事。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 頼みのあとの様子。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
}

impl Reply {
    pub fn ok(state: State) -> Self {
        Self {
            ok: true,
            error: None,
            state: Some(state),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            state: None,
        }
    }
}

/// アプリの今の様子。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// 今開いているフォルダ（ディスク上の場所があれば）と、その名前の道のり（Root から）。
    pub folder: Option<PathBuf>,
    pub breadcrumb: Vec<String>,
    /// 今開いているフォルダの DB の `entry.id`（DB から出したものだけ）。
    pub entry: Option<i64>,
    pub dups_open: bool,
    /// 重複の一覧の状態（"not_yet"、"computing"、"ready"、"failed"）と組の数。
    pub dups_state: String,
    pub dups_groups: usize,
    /// 選んでいる組のハッシュ（16 進）。
    pub dups_selected: Option<String>,
    pub paused: bool,
    /// 読み込みの進み具合。
    pub folders_read: u64,
    pub folders_failed: u64,
    pub folders_queued: usize,
    pub reading: Option<PathBuf>,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub hashing: Option<PathBuf>,
    pub finished: bool,
}

/// アプリに届いた頼み。`reply` に返事を送る。
pub struct Incoming {
    pub command: Command,
    pub reply: Sender<Reply>,
}

/// 待ち始める。頼みが届くたびに `deliver` を呼ぶ（アプリはそこで起きて、`Incoming` を受け取る）。
/// `control.json` を `dir` に書き、その場所を返す（終わるときに `remove` で消す）。
pub fn listen(
    dir: &Path,
    deliver: impl Fn(Incoming) -> bool + Send + Sync + 'static,
) -> io::Result<PathBuf> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = Endpoint {
        port: listener.local_addr()?.port(),
        token: uuid::Uuid::new_v4().simple().to_string(),
        pid: std::process::id(),
    };
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    write_private(
        &tmp,
        &serde_json::to_vec(&endpoint).map_err(io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)?;
    let deliver = std::sync::Arc::new(deliver);
    std::thread::Builder::new()
        .name("lisa-remote".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let token = endpoint.token.clone();
                let deliver = deliver.clone();
                let _ = std::thread::Builder::new()
                    .name("lisa-remote-conn".into())
                    .spawn(move || serve(stream, &token, &*deliver));
            }
        })?;
    Ok(path)
}

/// 自分だけが読めるように書く（合言葉が入っているので）。
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// `control.json` を消す（自分の書いたものだけ）。
pub fn remove(path: &Path) {
    let mine = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Endpoint>(&b).ok())
        .is_some_and(|e| e.pid == std::process::id());
    if mine {
        let _ = std::fs::remove_file(path);
    }
}

fn serve(stream: TcpStream, token: &str, deliver: &(dyn Fn(Incoming) -> bool + Send + Sync)) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(error) => Reply::error(format!("頼みが読めません: {error}")),
            Ok(request) if request.token != token => {
                // 合言葉が違うつなぎは、それ以上相手にしない。
                let _ = send_line(&mut writer, &Reply::error("合言葉が違います"));
                return;
            }
            Ok(request) => {
                let (sender, receiver) = channel();
                if !deliver(Incoming {
                    command: request.command,
                    reply: sender,
                }) {
                    Reply::error("アプリが終わろうとしています")
                } else {
                    receiver
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap_or_else(|_| Reply::error("アプリから返事がありません"))
                }
            }
        };
        if send_line(&mut writer, &reply).is_err() {
            return;
        }
    }
}

fn send_line<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

/// 外から頼む側。`dir` の `control.json` を読んで、1 つ頼んで返事を待つ。
pub fn send(dir: &Path, command: Command) -> Result<Reply, String> {
    let path = dir.join(FILE_NAME);
    let bytes = std::fs::read(&path).map_err(|_| {
        "Lisa が起動していません（control.json がありません）。先にアプリを開いてください。"
            .to_string()
    })?;
    let endpoint: Endpoint = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} が読めません: {e}", path.display()))?;
    let stream = TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, endpoint.port).into(),
        Duration::from_secs(2),
    )
    .map_err(|_| "Lisa につながりません（終わっているかもしれません）。".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    send_line(
        &mut writer,
        &Request {
            token: endpoint.token,
            command,
        },
    )
    .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("Lisa から返事がありません: {e}"))?;
    serde_json::from_str(&line).map_err(|e| format!("返事が読めません: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("lisa-remote-test-{}", std::process::id()));
        let path = listen(&dir, |incoming| {
            let reply = match incoming.command {
                Command::State => Reply::ok(State {
                    dups_groups: 3,
                    ..Default::default()
                }),
                _ => Reply::error("no"),
            };
            incoming.reply.send(reply).is_ok()
        })
        .unwrap();
        let reply = send(&dir, Command::State).unwrap();
        assert!(reply.ok);
        assert_eq!(reply.state.unwrap().dups_groups, 3);
        let reply = send(&dir, Command::Pause { paused: true }).unwrap();
        assert!(!reply.ok);

        // 合言葉が違えば断る。
        let endpoint: Endpoint = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port)).unwrap();
        stream
            .write_all(b"{\"token\":\"x\",\"command\":\"state\"}\n")
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(line.contains("\"ok\":false"));

        remove(&path);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
