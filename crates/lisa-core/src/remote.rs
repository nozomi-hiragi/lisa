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
    /// カタログのパネルを開け閉めする。`select` は当たったフォルダの `entry.id`（パネルで選ぶ）。
    Catalog {
        open: bool,
        #[serde(default)]
        select: Option<i64>,
    },
    /// アプリの窓をアプリ自身で撮って、データのフォルダの `screenshots/<name>.png` に残す（画面収録の
    /// 許可が無くても撮れる）。`name` は英数字・`-`・`_` だけ。
    Capture { name: String },
    /// フォルダを先に読み直して（ハッシュして）もらう。
    Rescan { path: PathBuf },
    /// 裏の読み込みを一時停止する・再開する。
    Pause { paused: bool },
    /// アプリを終了する。
    Quit,
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: 描画・ナビ・場面・カメラの状態を返す。
    DebugState,
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: 指定フォルダを「読み直したら変わっていた」扱いにして
    /// 読む係から `Changed -> Listed -> Committed` を流す。
    DebugChanged { path: PathBuf },
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: ピンチ入力を送る（正で拡大、負で縮小）。
    DebugPinch {
        delta: f32,
        #[serde(default)]
        touching: Option<bool>,
        #[serde(default)]
        cursor: Option<[f32; 2]>,
    },
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: 2 本指パン入力を送る。
    /// `phase` は `"began"` / `"changed"` / `"ended"` / `"tap"`。
    DebugPan {
        dx: f32,
        dy: f32,
        phase: String,
        #[serde(default)]
        cursor: Option<[f32; 2]>,
    },
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: マウスホイール入力を送る（正で拡大、負で縮小）。
    DebugWheel {
        y: f32,
        #[serde(default)]
        cursor: Option<[f32; 2]>,
    },
    /// 検証用（`LISA_DEBUG_REMOTE=1` のときのみ）: クリックや親へ戻る操作を送る。
    /// `action` は `"up"` または `"click"`（`screen_pos` に論理ピクセル座標を指定）。
    DebugNav {
        action: String,
        #[serde(default)]
        screen_pos: Option<[f32; 2]>,
    },
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
    /// 検証用の描画状態（`Command::DebugState` の返事だけ）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<DebugState>,
}

impl Reply {
    pub fn ok(state: State) -> Self {
        Self {
            ok: true,
            error: None,
            state: Some(state),
            debug: None,
        }
    }

    pub fn ok_debug(state: State, debug: DebugState) -> Self {
        Self {
            ok: true,
            error: None,
            state: Some(state),
            debug: Some(debug),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            state: None,
            debug: None,
        }
    }
}

/// 画面レイヤー上の `SceneRoot` 1 つ分の検証用情報。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugSceneInfo {
    pub entity: u64,
    pub folder: usize,
    pub folder_name: String,
    pub is_current: bool,
    pub is_parent_scene: bool,
    pub has_fade: bool,
    pub fade_in: Option<bool>,
    pub fade_replaces: Option<u64>,
    pub children_count: usize,
}

/// 今の場面の直下タイルの検証用情報。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugTileRect {
    pub node: usize,
    pub name: String,
    pub is_dir: bool,
    pub has_image: bool,
    pub image_format: Option<String>,
    /// 本体（`frame`）の画面上の矩形 `[x, y, width, height]`（論理ピクセル、左上原点）。
    pub screen_rect: Option<[f32; 4]>,
}

/// 検証用の描画・ナビゲーション状態。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugState {
    pub nav_nodes: Vec<usize>,
    pub nav_names: Vec<String>,
    #[serde(default)]
    pub nav_display_names: Vec<String>,
    pub nav_entries: Vec<Option<i64>>,
    pub nav_top_path: Option<PathBuf>,
    pub nav_top_attached: bool,
    pub nav_top_in_layout: bool,
    pub breadcrumb: String,
    pub menu_breadcrumb: Option<String>,
    pub anchor_node: usize,
    pub anchor_name: String,
    pub anchor_attached: bool,
    pub camera_pos: [f32; 2],
    pub camera_pos_finite: bool,
    pub camera_scale: f32,
    pub camera_scale_finite: bool,
    pub fit_pos: [f32; 2],
    pub fit_scale: f32,
    pub fit_scale_finite: bool,
    pub parent_fit_scale: Option<f32>,
    pub root_fit_scale: f32,
    pub window_logical_size: [f32; 2],
    pub window_scale_factor: f32,
    pub main_scenes: usize,
    pub scenes: Vec<DebugSceneInfo>,
    pub current_scene_folder: Option<usize>,
    pub current_scene_folder_name: Option<String>,
    pub gesture_active: bool,
    pub gesture_has_parent_scene: bool,
    pub animating: bool,
    pub fades: usize,
    pub bake_state: String,
    pub folder_images_slots: usize,
    pub folder_images_ready: usize,
    pub tree_changed_pending: bool,
    pub jump_pending: bool,
    pub occluded: bool,
    pub child_tiles: Vec<DebugTileRect>,
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

/// `control.json` を読む。無いか壊れていれば None。
pub fn read_endpoint(dir: &Path) -> Option<Endpoint> {
    let bytes = std::fs::read(dir.join(FILE_NAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
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
