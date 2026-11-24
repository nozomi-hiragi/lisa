//! LAN の他の PC の Lisa を見つけ、ペアリングして、木の中で見られるようにする。
//!
//! - 設定の「LAN に公開」が 1 のあいだ、アプリの中で `lisa_mcp::HttpListener`（既定 `0.0.0.0:47800`）と
//!   UDP の返事（`0.0.0.0:47801`）を動かす。0 に戻したら止める。
//! - どの側も起動時と 30 秒ごとに `255.255.255.255:47801`（と `127.0.0.1:47801`）へ `hello` を送り、
//!   2 秒ほど返事を集める。
//! - 覚えている相手は `peers.json` に保存し、Root の下に端末として並べる。
//! - 相手のノードが見えるところに来るか開かれたら、裏のスレッドでペアリング（未ペアリングのとき）や
//!   `lisa_drives`、`lisa_list` を呼び、結果を木に足す。ファイルは動かさず、相手の DB を見るだけ。

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::window::RequestRedraw;
use bevy::winit::{EventLoopProxy, EventLoopProxyWrapper, WinitUserEvent};
use lisa_core::query::{Drive, Item};
use lisa_mcp::{
    DEFAULT_LINGER_SECS, DISCOVERY_PORT, HttpListener, ListenerHandle, PairReply, PairRequest,
    Peer, SIDECAR_PORT, Server, add_peer, generate_pair_code, is_hello_packet, load_lan_config,
    load_or_create_token, make_hello_packet, make_here_packet, new_uuid, parse_here_packet,
    read_sidecar, revoke_peer,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::layout::Layouts;
use crate::model::tree::{FileTree, LoadState, NodeId, RemoteDrive};
use crate::render::palette;
use crate::render::scene::LabelFont;
use crate::scan::TreeChanged;
use crate::settings::Settings;
use crate::ui::menu::{BAR_PX, PopupBody, spawn_menu};
use crate::view::Nav;

/// 覚えた相手を保存するファイル（自分の `data_dir` の中）。
const PEERS_FILE: &str = "peers.json";
/// 相手を探す間隔。
const DISCOVER_EVERY: Duration = Duration::from_secs(30);
/// UDP の返事を集める時間。
const DISCOVER_WAIT: Duration = Duration::from_secs(2);
/// ペアリングの返事を待つ最大時間。
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
/// ペアリングの順番待ちの列の上限。超えたらすぐ断る。
const MAX_PAIR_QUEUE: usize = 5;
/// 画面に出す端末の名前の文字数の上限。これより長ければ「…」で切る。
const MAX_NAME_CHARS: usize = 40;
/// `ensure_sidecar` の二重起動を防ぐための待ち時間。
const SIDECAR_SPAWN_COOLDOWN: Duration = Duration::from_secs(3);
/// `lisa_list` の 1 回の取得件数。
const LIST_LIMIT: usize = 1000;
/// 1 フレームで新しく頼む相手のフォルダ数の上限。
const FETCH_PER_FRAME: usize = 16;
/// 同時に頼んでおく数の上限。多すぎると相手が接続を切ることがあった（Windows で 10054 が並んだ）。
const MAX_IN_FLIGHT: usize = 6;

const PANEL_COLOR: Color = palette::base_100_alpha(0.95);
const TEXT_COLOR: Color = palette::BASE_CONTENT;
const DIM_COLOR: Color = palette::MUTED;
const BUTTON_COLOR: Color = palette::NEUTRAL;
const ALLOW_COLOR: Color = palette::SUCCESS;
const ALLOW_TEXT_COLOR: Color = palette::SUCCESS_CONTENT;
const REJECT_COLOR: Color = palette::ERROR;
const REJECT_TEXT_COLOR: Color = palette::ERROR_CONTENT;

/// 自分が覚えておく相手（`peers.json` の 1 行）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedPeer {
    pub device_id: String,
    pub name: String,
    pub key: String,
    pub addr: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_port: Option<u16>,
}

/// UDP で見つけた相手。
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub device_id: String,
    pub name: String,
    pub addr: IpAddr,
    pub port: u16,
    pub mcp_port: Option<u16>,
    #[allow(dead_code)]
    pub last_seen: Instant,
}

pub struct LanPlugin;

impl Plugin for LanPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RemoteRefresh>();
        if std::env::var_os("LISA_DUMMY").is_some() {
            return;
        }
        app.add_systems(Startup, setup_lan)
            .add_systems(PostStartup, spawn_lan_ui.after(spawn_menu))
            .add_systems(
                Update,
                (
                    sync_share_listener,
                    poll_discovery,
                    poll_incoming_pairs,
                    poll_worker_events,
                    request_remote_nodes,
                    update_lan_ui,
                )
                    .chain()
                    .before(crate::view::ViewSystems),
            )
            .add_systems(Last, stop_on_exit);
    }
}

/// 動いている公開用の待ち受け（HTTP と UDP）。
struct RunningListener {
    http: ListenerHandle,
    stop_udp: Arc<AtomicBool>,
    udp_port: u16,
    http_port: u16,
}

impl RunningListener {
    fn stop(&self) {
        self.stop_udp.store(true, Ordering::Relaxed);
        self.http.unblock();
        if let Ok(sock) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = sock.send_to(b"stop", (Ipv4Addr::LOCALHOST, self.udp_port));
        }
    }
}

/// 相手からのペアリングの頼み（1 つずつ順番に画面に出す）。
struct PendingIncomingPair {
    request: PairRequest,
    /// 画面に出した時刻（60 秒はここから数える）。順番待ちの列にある間は `None`。
    shown_at: Option<Instant>,
    reply: Sender<Option<PairReply>>,
}

/// 端末の名前から制御文字（改行・タブなど）を取り除き、前後の空白を削って、`MAX_NAME_CHARS` 文字を超えたら
/// 「…」で切る。
pub(crate) fn clean_peer_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    let count = trimmed.chars().count();
    if count <= MAX_NAME_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_NAME_CHARS).collect();
    format!("{head}…")
}

/// 頼む側・受ける側の全体の状態。
#[derive(Resource)]
pub(crate) struct LanState {
    data_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    this_device_id: String,
    this_device_name: String,
    proxy: EventLoopProxy<WinitUserEvent>,
    /// 動かしている公開用の待ち受け。
    listener: Option<RunningListener>,
    /// 届いたペアリングの頼みを受け取る口と、順番待ちの列、今画面に出している 1 件。
    pair_rx: Mutex<Receiver<(PairRequest, Sender<Option<PairReply>>)>>,
    pair_tx: Sender<(PairRequest, Sender<Option<PairReply>>)>,
    pair_queue: VecDeque<PendingIncomingPair>,
    active_pair: Option<PendingIncomingPair>,
    /// 最後に UDP の `hello` を送った時刻と、その返事を受け取る口。
    last_discover: Option<Instant>,
    discovering: bool,
    discover_rx: Mutex<Receiver<Vec<DiscoveredPeer>>>,
    discover_tx: Sender<Vec<DiscoveredPeer>>,
    /// 見つけた相手（`device_id` → 情報）。
    discovered: HashMap<String, DiscoveredPeer>,
    /// 覚えている相手（`peers.json`）。
    saved: Vec<SavedPeer>,
    /// 相手への HTTP の頼みの結果を受け取る口。
    worker_rx: Mutex<Receiver<WorkerEvent>>,
    worker_tx: Sender<WorkerEvent>,
    /// 今裏で処理しているノード（二重に頼まないように）。
    in_flight: HashSet<NodeId>,
    /// 今ペアリングを頼んでいる相手と 4 桁の番号: `(device_id, name, code)`。
    outgoing_pair: Option<(String, String, String)>,
    /// 直前に見たナビゲーションの位置（開いたノードを検知するため）。
    last_top: Option<NodeId>,
    /// 設定のポップアップに出す「つないだ端末」（`lan.json` の `peers`）のキャッシュ。
    authorized_peers: Vec<Peer>,
    authorized_dirty: bool,
}

impl LanState {
    pub(crate) fn relay_port(&self) -> Option<u16> {
        self.listener.as_ref().map(|listener| listener.http_port)
    }

    pub(crate) fn paired_devices(&self) -> impl Iterator<Item = (&str, &str)> {
        self.saved
            .iter()
            .map(|peer| (peer.device_id.as_str(), peer.name.as_str()))
    }

    pub(crate) fn is_this_device(&self, id: &str) -> bool {
        self.this_device_id == id
    }

    /// ペアリング済みの相手を `device_id` で引く。
    pub(crate) fn saved_peer(&self, device_id: &str) -> Option<SavedPeer> {
        self.saved
            .iter()
            .find(|s| s.device_id == device_id)
            .cloned()
    }
}

enum WorkerEvent {
    /// ペアリングが済んだ（成功なら `Ok(SavedPeer)`、断られた・つながらないなら `Err`）。
    Paired {
        node: NodeId,
        device_id: String,
        result: Result<SavedPeer, String>,
    },
    /// `lisa_drives` の結果。`unauthorized` が true なら 401 だったので鍵を捨てる。
    Drives {
        node: NodeId,
        device_id: String,
        result: Result<Vec<RemoteDrive>, (bool, String)>,
    },
    /// `lisa_list` の結果。
    Children {
        node: NodeId,
        device_id: String,
        result: Result<Vec<Item>, (bool, String)>,
    },
}

/// `peers.json` を読む。無いか壊れていれば空。
pub fn load_saved_peers(data_dir: &Path) -> Vec<SavedPeer> {
    let path = data_dir.join(PEERS_FILE);
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<SavedPeer>>(&bytes).ok())
        .unwrap_or_default()
}

/// `peers.json` を保存する。
pub fn save_saved_peers(data_dir: &Path, peers: &[SavedPeer]) {
    let _ = std::fs::create_dir_all(data_dir);
    let path = data_dir.join(PEERS_FILE);
    let tmp = data_dir.join(format!("{PEERS_FILE}.tmp"));
    if let Ok(bytes) = serde_json::to_vec_pretty(peers) {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&tmp) {
            use std::io::Write;
            if file.write_all(&bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// この端末の `(device_id, name)` を DB から取る（DB が無ければ作る）。
fn read_this_device(data_dir: &Path) -> (String, String) {
    let fallback_name = lisa_core::volumes::device_name();
    let db_path = data_dir.join("lisa.db");
    if let Ok(mut db) = lisa_core::db::Db::open(&db_path)
        && let Ok(id) = db.this_device(&fallback_name, &fallback_name, std::env::consts::OS)
    {
        let name = lisa_core::query::this_device(&db)
            .ok()
            .flatten()
            .map(|(n, _)| n)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(fallback_name);
        return (id, name);
    }
    (new_uuid(), fallback_name)
}

fn setup_lan(
    mut commands: Commands,
    proxy: Res<EventLoopProxyWrapper>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
) {
    let Some(data_dir) = crate::scan::data_dir() else {
        return;
    };
    let cache_dir = crate::scan::cache_dir();
    let (this_device_id, this_device_name) = read_this_device(&data_dir);
    let saved: Vec<SavedPeer> = load_saved_peers(&data_dir)
        .into_iter()
        .map(|mut p| {
            p.name = clean_peer_name(&p.name);
            p
        })
        .collect();
    // 覚えている相手は、まだ見つかっていなくても「つながっていない（Unloaded）」として木に出しておく。
    for peer in &saved {
        tree.ensure_remote_device(&peer.device_id, &peer.name, LoadState::Unloaded);
    }
    if !saved.is_empty() {
        changed.pending = true;
    }
    let authorized_peers = load_clean_peers(&data_dir);
    let (pair_tx, pair_rx) = channel();
    let (discover_tx, discover_rx) = channel();
    let (worker_tx, worker_rx) = channel();
    commands.insert_resource(LanState {
        data_dir,
        cache_dir,
        this_device_id,
        this_device_name,
        proxy: (**proxy).clone(),
        listener: None,
        pair_rx: Mutex::new(pair_rx),
        pair_tx,
        pair_queue: VecDeque::new(),
        active_pair: None,
        last_discover: None,
        discovering: false,
        discover_rx: Mutex::new(discover_rx),
        discover_tx,
        discovered: HashMap::new(),
        saved,
        worker_rx: Mutex::new(worker_rx),
        worker_tx,
        in_flight: HashSet::new(),
        outgoing_pair: None,
        last_top: None,
        authorized_peers,
        authorized_dirty: true,
    });
}

fn load_clean_peers(data_dir: &Path) -> Vec<Peer> {
    load_lan_config(data_dir)
        .peers
        .into_iter()
        .map(|mut p| {
            p.name = clean_peer_name(&p.name);
            p
        })
        .collect()
}

/// 最後に `ensure_sidecar_in` で `lisa-mcp` を立ち上げた時刻（短時間の二重起動を防ぐ）。
static LAST_SIDECAR_SPAWN: Mutex<Option<Instant>> = Mutex::new(None);
/// 起動時（`LogPlugin` より前）の `ensure_sidecar` の結果を、あとで `lisa.log` に出すために覚えておく。
static STARTUP_SIDECAR_LOG: Mutex<Option<Result<String, String>>> = Mutex::new(None);

/// 設定の「LAN に公開」が 1 のとき、アプリの exe と同じフォルダの `lisa-mcp` を別プロセスとして
/// 立ち上げる（すでに動いていれば何もしない）。Bevy より前（`main.rs`）と、実行中に設定を変えたときの
/// 両方から呼ぶ。
pub fn ensure_sidecar() {
    let Some(data_dir) = crate::scan::data_dir() else {
        return;
    };
    let res = ensure_sidecar_in(&data_dir);
    if let Ok(mut slot) = STARTUP_SIDECAR_LOG.lock() {
        *slot = Some(res);
    }
}

/// 起動時の `ensure_sidecar` の結果を取り出す（`main.rs` の `log_startup` で `lisa.log` に書く）。
pub fn take_startup_sidecar_log() -> Option<Result<String, String>> {
    STARTUP_SIDECAR_LOG.lock().ok().and_then(|mut s| s.take())
}

/// 裏のスレッドで `ensure_sidecar_in` を動かし、終わった結果を `lisa.log` に出す（メインスレッドを止めないため）。
fn spawn_ensure_sidecar(data_dir: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("lisa-sidecar-start".into())
        .spawn(move || match ensure_sidecar_in(&data_dir) {
            Ok(msg) => info!("{msg}"),
            Err(err) => warn!("{err}"),
        });
}

/// `<data_dir>/sidecar.json` の pid に対して `/health` が返ることを確かめてから sidecar を止める（裏のスレッドで）。
fn spawn_stop_sidecar(data_dir: PathBuf) {
    let _ = std::thread::Builder::new()
        .name("lisa-sidecar-stop".into())
        .spawn(move || stop_sidecar_in(&data_dir));
}

fn stop_sidecar_in(data_dir: &Path) {
    if let Ok(mut last) = LAST_SIDECAR_SPAWN.lock() {
        *last = None;
    }
    let Some(info) = read_sidecar(data_dir) else {
        return;
    };
    if info.pid == 0 || info.port == 0 || info.pid == std::process::id() {
        return;
    }
    if sidecar_healthy(data_dir).is_none() {
        return;
    }
    match lisa_mcp::terminate_process(info.pid) {
        Ok(()) => {
            lisa_mcp::remove_sidecar(data_dir, info.pid);
            info!("lisa-mcp（pid={}）を止めました", info.pid);
        }
        Err(error) => {
            warn!(
                "lisa-mcp（pid={}）を止められませんでした: {error}",
                info.pid
            );
        }
    }
}

fn ensure_sidecar_in(data_dir: &Path) -> Result<String, String> {
    if let Ok(guard) = LAST_SIDECAR_SPAWN.lock()
        && let Some(last) = *guard
        && last.elapsed() < SIDECAR_SPAWN_COOLDOWN
    {
        return Ok("lisa-mcp は立ち上げ中です".into());
    }
    if let Some(port) = sidecar_healthy(data_dir) {
        return Ok(format!("lisa-mcp はすでに動いています（port={port}）"));
    }
    if let Ok(mut guard) = LAST_SIDECAR_SPAWN.lock() {
        if let Some(last) = *guard
            && last.elapsed() < SIDECAR_SPAWN_COOLDOWN
        {
            return Ok("lisa-mcp は立ち上げ中です".into());
        }
        *guard = Some(Instant::now());
    }
    let exe_name = if cfg!(windows) {
        "lisa-mcp.exe"
    } else {
        "lisa-mcp"
    };
    let Some(mcp_exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe_name)))
    else {
        let msg = "lisa-mcp の場所が分かりませんでした".to_string();
        if let Ok(mut guard) = LAST_SIDECAR_SPAWN.lock() {
            *guard = None;
        }
        eprintln!("{msg}");
        return Err(msg);
    };
    if !mcp_exe.exists() {
        let msg = format!("lisa-mcp が見つかりません（{}）", mcp_exe.display());
        if let Ok(mut guard) = LAST_SIDECAR_SPAWN.lock() {
            *guard = None;
        }
        eprintln!("{msg}");
        return Err(msg);
    }
    let mut cmd = std::process::Command::new(&mcp_exe);
    cmd.arg("--listen")
        .arg(format!("0.0.0.0:{SIDECAR_PORT}"))
        .arg("--parent")
        .arg(std::process::id().to_string())
        .arg("--linger")
        .arg(DEFAULT_LINGER_SECS.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = std::env::var_os("LISA_DATA_DIR").filter(|s| !s.is_empty()) {
        cmd.arg("--data-dir").arg(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x0800_0000) | DETACHED_PROCESS (0x0000_0008)
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            #[cfg(windows)]
            let _ = &mut child;
            #[cfg(not(windows))]
            {
                // Unix では子が終わったあとにゾンビとして残らないよう、別スレッドで回収する。
                let _ = std::thread::Builder::new()
                    .name("lisa-sidecar-reap".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            }
            Ok(format!(
                "lisa-mcp を立ち上げました（pid={pid}, exe={}）",
                mcp_exe.display()
            ))
        }
        Err(error) => {
            if let Ok(mut guard) = LAST_SIDECAR_SPAWN.lock() {
                *guard = None;
            }
            let msg = format!(
                "lisa-mcp を立ち上げられませんでした（{}）: {error}",
                mcp_exe.display()
            );
            eprintln!("{msg}");
            Err(msg)
        }
    }
}

/// `<data_dir>/sidecar.json` に pid と port があり、`127.0.0.1:<port>/health` が 200 で返ればその port。
fn sidecar_healthy(data_dir: &Path) -> Option<u16> {
    let info = read_sidecar(data_dir)?;
    if info.pid == 0 || info.port == 0 {
        return None;
    }
    let url = format!("http://127.0.0.1:{}/health", info.port);
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_millis(500)))
        .timeout_recv_response(Some(Duration::from_millis(500)))
        .timeout_recv_body(Some(Duration::from_millis(500)))
        .http_status_as_error(false)
        .build();
    let agent: ureq::Agent = agent.into();
    let response = agent.get(&url).call().ok()?;
    (response.status().as_u16() == 200).then_some(info.port)
}

/// 設定の「LAN に公開」に合わせて、アプリの中の `HttpListener`・UDP の返事と sidecar を立ち上げ・停止する。
fn sync_share_listener(settings: Res<Settings>, lan: Option<ResMut<LanState>>) {
    let Some(mut lan) = lan else {
        return;
    };
    let want = settings.lan_share >= 0.5;
    if want && lan.listener.is_none() {
        spawn_ensure_sidecar(lan.data_dir.clone());
        match start_listener(&lan) {
            Ok(running) => lan.listener = Some(running),
            Err(error) => warn!("LAN の公開を始められませんでした: {error}"),
        }
    } else if !want && let Some(running) = lan.listener.take() {
        running.stop();
        spawn_stop_sidecar(lan.data_dir.clone());
        info!("LAN の公開を止めました");
    }
}

fn start_listener(lan: &LanState) -> Result<RunningListener, String> {
    let token = load_or_create_token(&lan.data_dir)?;
    let make_server = || Server {
        data_dir: lan.data_dir.clone(),
        cache_dir: lan.cache_dir.clone(),
    };
    let listener = HttpListener::bind(lisa_mcp::DEFAULT_LISTEN, make_server(), token.clone())
        .or_else(|_| HttpListener::bind("0.0.0.0:0", make_server(), token))?;
    let http_port = listener.local_addr().port();
    let http_handle = listener.handle();
    let pair_tx = lan.pair_tx.clone();
    let proxy = lan.proxy.clone();
    let listener = listener.with_pair_handler(Box::new(move |req| {
        let (tx, rx) = channel();
        if pair_tx.send((req, tx)).is_err() {
            return None;
        }
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
        rx.recv_timeout(PAIR_TIMEOUT).ok().flatten()
    }));
    std::thread::Builder::new()
        .name("lisa-lan-http".into())
        .spawn(move || listener.run())
        .map_err(|e| e.to_string())?;

    let udp_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))
        .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
        .map_err(|e| format!("UDP で待てませんでした: {e}"))?;
    let udp_port = udp_sock.local_addr().map_err(|e| e.to_string())?.port();
    let _ = udp_sock.set_read_timeout(Some(Duration::from_millis(500)));
    let stop_udp = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop_udp);
    let device_id = lan.this_device_id.clone();
    let device_name = lan.this_device_name.clone();
    let data_dir = lan.data_dir.clone();
    std::thread::Builder::new()
        .name("lisa-lan-udp".into())
        .spawn(move || {
            let mut buf = [0u8; 512];
            while !stop_flag.load(Ordering::Relaxed) {
                let Ok((n, src)) = udp_sock.recv_from(&mut buf) else {
                    continue;
                };
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                if is_hello_packet(&buf[..n]) {
                    let mcp_port = read_sidecar(&data_dir)
                        .map(|s| s.port)
                        .filter(|&p| p != 0)
                        .or(Some(SIDECAR_PORT));
                    let reply = make_here_packet(&device_id, &device_name, http_port, mcp_port);
                    let _ = udp_sock.send_to(&reply, src);
                }
            }
        })
        .map_err(|e| e.to_string())?;

    info!("LAN に公開しています（HTTP:{http_port}, UDP:{udp_port}）");
    Ok(RunningListener {
        http: http_handle,
        stop_udp,
        udp_port,
        http_port,
    })
}

fn stop_on_exit(mut exit: MessageReader<AppExit>, lan: Option<ResMut<LanState>>) {
    if exit.read().next().is_some()
        && let Some(mut lan) = lan
        && let Some(running) = lan.listener.take()
    {
        running.stop();
    }
}

/// 起動時と 30 秒ごとに UDP で `hello` を送り、見つけた相手を木に反映する（操作中は保留）。
fn poll_discovery(
    lan: Option<ResMut<LanState>>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    // 合成端末での画面検証では、実際の LAN に問い合わせない。
    if crate::remote::debug_remote_enabled()
        && std::env::var("LISA_DEBUG_LAN_DISCOVERY").as_deref() == Ok("0")
    {
        return;
    }
    let Some(mut lan) = lan else {
        return;
    };
    if animating.is_some() || !fades.is_empty() || gesture.active() {
        return;
    }
    let lan = &mut *lan;
    if let Ok(rx) = lan.discover_rx.lock() {
        while let Ok(found) = rx.try_recv() {
            lan.discovering = false;
            // 逆向きにペア登録されていない依頼元にも、ローカル CLI から最新の発見結果を使える。
            // 接続先の応答はペアリング鍵で照合するので、発見パケットだけで実行を許可しない。
            let seen_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let path = lan
                .data_dir
                .join(lisa_core::organize::remote_keep::relay::DISCOVERY_FILE);
            // 1 回の発見で取りこぼした相手も、前回の記録（期限は読む側が確かめる）を残す。
            // 上書きすると、長い転送の途中で中継端末を見失って止まる。
            let mut relay_endpoints: serde_json::Map<String, Value> = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            relay_endpoints.retain(|device, _| {
                lan.authorized_peers
                    .iter()
                    .any(|allowed| &allowed.device_id == device)
            });
            relay_endpoints.extend(
                found
                    .iter()
                    .filter(|peer| {
                        lan.authorized_peers
                            .iter()
                            .any(|allowed| allowed.device_id == peer.device_id)
                    })
                    .map(|peer| {
                        (
                            peer.device_id.clone(),
                            json!({
                                "address": SocketAddr::new(peer.addr,peer.port),
                                "seen_at": seen_at,
                            }),
                        )
                    }),
            );
            let tmp = path.with_extension("tmp");
            if let Ok(bytes) = serde_json::to_vec(&relay_endpoints)
                && std::fs::write(&tmp, bytes).is_ok()
            {
                let _ = std::fs::rename(tmp, path);
            }
            let mut any_saved_changed = false;
            for peer in found {
                // 覚えている相手の IP やポートが変わっていたら更新して保存する。
                let addr_str = peer.addr.to_string();
                if let Some(saved) = lan.saved.iter_mut().find(|s| s.device_id == peer.device_id)
                    && (saved.addr != addr_str
                        || saved.port != peer.port
                        || saved.name != peer.name
                        || (peer.mcp_port.is_some() && saved.mcp_port != peer.mcp_port))
                {
                    saved.addr = addr_str;
                    saved.port = peer.port;
                    saved.name = peer.name.clone();
                    if peer.mcp_port.is_some() {
                        saved.mcp_port = peer.mcp_port;
                    }
                    any_saved_changed = true;
                }
                tree.ensure_remote_device(&peer.device_id, &peer.name, LoadState::Unloaded);
                lan.discovered.insert(peer.device_id.clone(), peer);
                changed.pending = true;
            }
            if any_saved_changed {
                save_saved_peers(&lan.data_dir, &lan.saved);
            }
        }
    }
    let due = lan
        .last_discover
        .is_none_or(|t| t.elapsed() >= DISCOVER_EVERY);
    if !lan.discovering && due {
        lan.last_discover = Some(Instant::now());
        lan.discovering = true;
        let tx = lan.discover_tx.clone();
        let proxy = lan.proxy.clone();
        let own_id = lan.this_device_id.clone();
        let _ = std::thread::Builder::new()
            .name("lisa-lan-discover".into())
            .spawn(move || {
                let found = run_discovery(&own_id);
                let _ = tx.send(found);
                let _ = proxy.send_event(WinitUserEvent::WakeUp);
            });
    }
}

/// 既定の経路で外へ出るときの、この PC の IPv4 アドレス（パケットは送らない）。
fn primary_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // 経路を決めるだけで、UDP の connect は何も送らない。
    sock.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

fn run_discovery(own_device_id: &str) -> Vec<DiscoveredPeer> {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let hello = make_hello_packet();
    let mut sockets = vec![sock];
    // Windows は 255.255.255.255 宛てを、リンクローカルや VPN など LAN でないアダプタから出すことが
    // ある。既定の経路のアドレスに結び付けたソケットからも送り、LAN に確実に届ける。
    if let Some(ip) = primary_ipv4()
        && let Ok(bound) = UdpSocket::bind((ip, 0))
    {
        sockets.push(bound);
    }
    for sock in &sockets {
        let _ = sock.set_broadcast(true);
        let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
        let _ = sock.send_to(&hello, (Ipv4Addr::BROADCAST, DISCOVERY_PORT));
    }
    let _ = sockets[0].send_to(&hello, (Ipv4Addr::LOCALHOST, DISCOVERY_PORT));

    let started = Instant::now();
    let mut map: HashMap<String, DiscoveredPeer> = HashMap::new();
    let mut buf = [0u8; 512];
    let mut turn = 0usize;
    while started.elapsed() < DISCOVER_WAIT {
        let sock = &sockets[turn % sockets.len()];
        turn += 1;
        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Some((device_id, name, port, mcp_port)) = parse_here_packet(&buf[..n]) else {
            continue;
        };
        if device_id == own_device_id {
            continue;
        }
        let name = clean_peer_name(&name);
        if name.is_empty() {
            continue;
        }
        map.insert(
            device_id.clone(),
            DiscoveredPeer {
                device_id,
                name,
                addr: src.ip(),
                port,
                mcp_port,
                last_seen: Instant::now(),
            },
        );
    }
    map.into_values().collect()
}

/// 届いたペアリングの頼みを `active_pair` と `pair_queue` に受け入れる。
///
/// - 同じ `device_id` の古い頼み（`active_pair` または `pair_queue` にあるもの）は `None` を返して置き換える。
/// - 順番待ちの列が `MAX_PAIR_QUEUE` 件に達していたら、新しい頼みはその場で `None` を返して断る。
/// - 60 秒の制限時間は画面に出した時点（`active_pair` に入った時点）から数える。
///
/// 画面の表示が変わったら true を返す。
fn step_pair_queue<I>(
    active_pair: &mut Option<PendingIncomingPair>,
    pair_queue: &mut VecDeque<PendingIncomingPair>,
    incoming: I,
    now: Instant,
) -> bool
where
    I: IntoIterator<Item = (PairRequest, Sender<Option<PairReply>>)>,
{
    let mut changed = false;
    for (mut request, reply) in incoming {
        request.name = clean_peer_name(&request.name);
        if request.name.is_empty() || request.device_id.trim().is_empty() {
            let _ = reply.send(None);
            continue;
        }
        // 同じ device_id の古い頼みが順番待ちにあれば断って取り除く。
        let mut i = 0;
        while i < pair_queue.len() {
            if pair_queue[i].request.device_id == request.device_id {
                if let Some(old) = pair_queue.remove(i) {
                    let _ = old.reply.send(None);
                }
            } else {
                i += 1;
            }
        }
        // 今画面に出している頼みが同じ device_id なら、それを断って新しい頼みに差し替える。
        if active_pair
            .as_ref()
            .is_some_and(|a| a.request.device_id == request.device_id)
        {
            if let Some(old) = active_pair.take() {
                let _ = old.reply.send(None);
            }
            *active_pair = Some(PendingIncomingPair {
                request,
                shown_at: Some(now),
                reply,
            });
            changed = true;
            continue;
        }
        if pair_queue.len() >= MAX_PAIR_QUEUE {
            let _ = reply.send(None);
            continue;
        }
        pair_queue.push_back(PendingIncomingPair {
            request,
            shown_at: None,
            reply,
        });
        changed = true;
    }
    if let Some(active) = active_pair.as_ref()
        && active
            .shown_at
            .is_some_and(|t| now.saturating_duration_since(t) >= PAIR_TIMEOUT)
    {
        if let Some(expired) = active_pair.take() {
            let _ = expired.reply.send(None);
        }
        changed = true;
    }
    if active_pair.is_none()
        && let Some(mut next) = pair_queue.pop_front()
    {
        next.shown_at = Some(now);
        *active_pair = Some(next);
        changed = true;
    }
    changed
}

/// 相手から届いた `POST /pair` の頼みを順番に 1 つずつ出し、画面に出してから 60 秒過ぎたら断る。
fn poll_incoming_pairs(lan: Option<ResMut<LanState>>, mut redraw: MessageWriter<RequestRedraw>) {
    let Some(mut lan) = lan else {
        return;
    };
    let lan = &mut *lan;
    let incoming: Vec<(PairRequest, Sender<Option<PairReply>>)> = match lan.pair_rx.lock() {
        Ok(rx) => rx.try_iter().collect(),
        Err(_) => Vec::new(),
    };
    if step_pair_queue(
        &mut lan.active_pair,
        &mut lan.pair_queue,
        incoming,
        Instant::now(),
    ) {
        redraw.write(RequestRedraw);
    }
}

/// Root の下にある該当 `device_id` の端末ノードを探す。
fn find_remote_device_node(tree: &FileTree, device_id: &str) -> Option<NodeId> {
    tree.nodes[FileTree::ROOT]
        .children
        .iter()
        .copied()
        .find(|&c| {
            tree.nodes[c]
                .remote
                .as_ref()
                .is_some_and(|r| r.device_id == device_id && r.entry_id.is_none())
        })
}

/// 相手にペアリングを取り消された（401）とき、端末ノードの子を外して `Failed`（`loaded = false`）に戻し、
/// 端末ノードを開き直せば再ペアリング・ドライブ再取得ができるようにする。
fn reset_unauthorized_device(
    tree: &mut FileTree,
    in_flight: &mut HashSet<NodeId>,
    device_id: &str,
) {
    if let Some(dev_node) = find_remote_device_node(tree, device_id) {
        in_flight.remove(&dev_node);
        tree.reset_children(dev_node);
        tree.set_remote_state(dev_node, LoadState::Failed, false);
    }
}

/// 裏のスレッドから届いたペアリング・ドライブ・フォルダの結果を木に反映する（操作中は保留）。
#[allow(clippy::too_many_arguments)]
fn poll_worker_events(
    lan: Option<ResMut<LanState>>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    mut sync: ResMut<crate::view::SyncRequest>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Some(mut lan) = lan else {
        return;
    };
    if animating.is_some() || !fades.is_empty() || gesture.active() {
        return;
    }
    let lan = &mut *lan;
    let events: Vec<WorkerEvent> = match lan.worker_rx.lock() {
        Ok(rx) => rx.try_iter().collect(),
        Err(_) => return,
    };
    if events.is_empty() {
        return;
    }
    for event in events {
        match event {
            WorkerEvent::Paired {
                node,
                device_id,
                result,
            } => {
                lan.in_flight.remove(&node);
                if lan
                    .outgoing_pair
                    .as_ref()
                    .is_some_and(|(id, ..)| *id == device_id)
                {
                    lan.outgoing_pair = None;
                }
                match result {
                    Ok(peer) => {
                        tree.nodes[node].name = peer.name.clone();
                        if let Some(existing) =
                            lan.saved.iter_mut().find(|s| s.device_id == device_id)
                        {
                            *existing = peer.clone();
                        } else {
                            lan.saved.push(peer.clone());
                        }
                        save_saved_peers(&lan.data_dir, &lan.saved);
                        // ペアリングできたら、続けてドライブを取りに行く。
                        lan.in_flight.insert(node);
                        tree.set_remote_state(node, LoadState::Loading, false);
                        spawn_fetch_drives(node, peer, lan.worker_tx.clone(), lan.proxy.clone());
                    }
                    Err(error) => {
                        if lan.saved.iter().any(|s| s.device_id == device_id) {
                            // すでにペアリング済み（双方から同時に開いたときなど）なら失敗にしない。
                            continue;
                        }
                        warn!("ペアリングできませんでした（{device_id}）: {error}");
                        tree.set_remote_state(node, LoadState::Failed, false);
                    }
                }
                changed.pending = true;
                redraw.write(RequestRedraw);
            }
            WorkerEvent::Drives {
                node,
                device_id,
                result,
            } => {
                lan.in_flight.remove(&node);
                match result {
                    Ok(drives) => {
                        tree.add_remote_drives(node, drives);
                    }
                    Err((unauthorized, error)) => {
                        warn!("相手のドライブを取れませんでした（{device_id}）: {error}");
                        if unauthorized {
                            lan.saved.retain(|s| s.device_id != device_id);
                            save_saved_peers(&lan.data_dir, &lan.saved);
                            reset_unauthorized_device(&mut tree, &mut lan.in_flight, &device_id);
                            sync.request_reset();
                        } else {
                            tree.set_remote_state(node, LoadState::Failed, false);
                        }
                    }
                }
                changed.pending = true;
                redraw.write(RequestRedraw);
            }
            WorkerEvent::Children {
                node,
                device_id,
                result,
            } => {
                lan.in_flight.remove(&node);
                match result {
                    Ok(items) => {
                        tree.add_remote_children(node, items);
                    }
                    Err((unauthorized, error)) => {
                        warn!("相手のフォルダを読めませんでした（{device_id}）: {error}");
                        if unauthorized {
                            lan.saved.retain(|s| s.device_id != device_id);
                            save_saved_peers(&lan.data_dir, &lan.saved);
                            tree.set_remote_state(node, LoadState::Failed, true);
                            reset_unauthorized_device(&mut tree, &mut lan.in_flight, &device_id);
                            sync.request_reset();
                        } else {
                            tree.set_remote_state(node, LoadState::Failed, true);
                        }
                    }
                }
                changed.pending = true;
                redraw.write(RequestRedraw);
            }
        }
    }
}

/// 重複一覧から相手の階層を開く。足りない段は既存の LAN 読み取りを使う。
#[derive(Resource, Default)]
pub(crate) struct RemoteRefresh(pub std::collections::HashSet<String>);

#[derive(Resource)]
pub(crate) struct RemoteJump {
    pub device: String,
    pub chain: Vec<i64>,
}

/// 相手の端末やフォルダが見えるところに来るか開かれたら、裏のスレッドで中身を取りに行く（操作中は保留）。
#[allow(clippy::too_many_arguments)]
fn request_remote_nodes(
    lan: Option<ResMut<LanState>>,
    jump: Option<Res<RemoteJump>>,
    mut refresh: ResMut<RemoteRefresh>,
    mut sync: ResMut<crate::view::SyncRequest>,
    mut commands: Commands,
    nav: Res<Nav>,
    layouts: Res<Layouts>,
    mut tree: ResMut<FileTree>,
    mut changed: ResMut<TreeChanged>,
    animating: Option<Res<crate::view::Animating>>,
    fades: Query<(), With<crate::view::Fade>>,
    gesture: Res<crate::input::gesture::Gesture>,
) {
    let Some(mut lan) = lan else {
        return;
    };
    if animating.is_some() || !fades.is_empty() || gesture.active() {
        return;
    }
    let lan = &mut *lan;
    let mut refreshed = Vec::new();
    for &device_node in &tree.nodes[FileTree::ROOT].children {
        let Some(reference) = &tree.nodes[device_node].remote else {
            continue;
        };
        if refresh.0.contains(&reference.device_id)
            && !lan.in_flight.iter().any(|&node| {
                tree.nodes[node]
                    .remote
                    .as_ref()
                    .is_some_and(|other| other.device_id == reference.device_id)
            })
        {
            refreshed.push((device_node, reference.device_id.clone()));
        }
    }
    for (node, device) in refreshed {
        tree.reset_children(node);
        tree.set_remote_state(node, LoadState::Unloaded, false);
        tree.preview_revision = tree.preview_revision.wrapping_add(1);
        if !tree.is_attached(nav.top().node) || !tree.is_attached(layouts.anchor) {
            sync.request_reset();
        }
        changed.pending = true;
        refresh.0.remove(&device);
    }
    refresh
        .0
        .retain(|device| lan.saved.iter().any(|peer| peer.device_id == *device));
    let top = nav.top().node;
    if !tree.is_attached(top) {
        return;
    }
    let opened_new = lan.last_top != Some(top);
    lan.last_top = Some(top);

    // 失敗した相手のノード（端末もフォルダも）は、もう一度開いたら再び試せるようにする
    // （つながらなかった・切られたなどは、すぐ直ることが多い）。
    // また、未ペアリングの相手の端末ノードが `Failed` になっていても、相手からペアリングが届いている最中は
    // 戻さず、相手の頼みを「許可」か「断る」まで待つ（双方から同時に開いたときに二重に頼まないように）。
    if opened_new
        && tree.nodes[top].remote.is_some()
        && tree.nodes[top].state == LoadState::Failed
        && !lan.in_flight.contains(&top)
    {
        tree.set_remote_state(top, LoadState::Unloaded, false);
        changed.pending = true;
    }

    let mut candidates: Vec<NodeId> = Vec::new();
    if let Some(jump) = jump {
        use crate::model::{display_path, tree::RemoteRef};
        if let Some(peer) = lan.saved_peer(&jump.device) {
            tree.ensure_remote_device(&peer.device_id, &peer.name, LoadState::Unloaded);
            let steps: Vec<_> = std::iter::once(None)
                .chain(jump.chain.iter().copied().map(Some))
                .map(|entry_id| {
                    display_path::Step::Remote(RemoteRef {
                        device_id: jump.device.clone(),
                        entry_id,
                    })
                })
                .collect();
            let path = display_path::resolve_steps(&tree, &steps);
            let deepest = *path.last().unwrap_or(&FileTree::ROOT);
            if path.len() == steps.len() + 1 || tree.nodes[deepest].loaded {
                commands.remove_resource::<RemoteJump>();
                commands.insert_resource(crate::ui::menu::BreadcrumbJump(steps));
            } else if tree.nodes[deepest].state == LoadState::Failed {
                commands.remove_resource::<RemoteJump>();
                commands.insert_resource(crate::ui::menu::BreadcrumbError(Instant::now()));
            } else {
                candidates.push(deepest);
            }
        } else {
            commands.remove_resource::<RemoteJump>();
        }
    }
    // 開いているノード自身を最優先にする。
    if tree.nodes[top].remote.is_some() {
        candidates.push(top);
    }
    for id in layouts.ids() {
        if id != top && tree.nodes[id].remote.is_some() {
            candidates.push(id);
        }
    }

    let mut started = 0;
    for id in candidates {
        if started >= FETCH_PER_FRAME || lan.in_flight.len() >= MAX_IN_FLIGHT {
            break;
        }
        if lan.in_flight.contains(&id) {
            continue;
        }
        let node = &tree.nodes[id];
        if !node.is_dir
            || node.loaded
            || !node.listed
            || node.state == LoadState::Failed
            || tree.marker_view_read_only(id)
        {
            continue;
        }
        let Some(remote) = node.remote.clone() else {
            continue;
        };
        match remote.entry_id {
            None => {
                // 端末のノード。Root の一覧で見えているだけではペアリングのポップアップを出さず、
                // 覚えている相手なら黙ってつなぎ、未ペアリングならその端末を開いたときに頼む。
                if let Some(peer) = lan
                    .saved
                    .iter()
                    .find(|s| s.device_id == remote.device_id)
                    .cloned()
                {
                    // 見つかっておらず、まだ一度もつないでいないか開かれてもいないときは、黙って試す。
                    lan.in_flight.insert(id);
                    tree.set_remote_state(id, LoadState::Loading, false);
                    spawn_fetch_drives(id, peer, lan.worker_tx.clone(), lan.proxy.clone());
                    changed.pending = true;
                    started += 1;
                } else if id == top {
                    // 同じ相手からすでにペアリングの頼みが届いている最中は、こちらからは重ねて送らない。
                    let incoming_from_same = lan
                        .active_pair
                        .as_ref()
                        .is_some_and(|a| a.request.device_id == remote.device_id)
                        || lan
                            .pair_queue
                            .iter()
                            .any(|q| q.request.device_id == remote.device_id);
                    if incoming_from_same {
                        continue;
                    }
                    let Some(disc) = lan.discovered.get(&remote.device_id).cloned() else {
                        tree.set_remote_state(id, LoadState::Failed, false);
                        changed.pending = true;
                        continue;
                    };
                    let code = generate_pair_code();
                    lan.outgoing_pair =
                        Some((disc.device_id.clone(), disc.name.clone(), code.clone()));
                    lan.in_flight.insert(id);
                    tree.set_remote_state(id, LoadState::Loading, false);
                    spawn_pair_request(
                        id,
                        disc,
                        lan.this_device_id.clone(),
                        lan.this_device_name.clone(),
                        code,
                        lan.worker_tx.clone(),
                        lan.proxy.clone(),
                    );
                    changed.pending = true;
                    started += 1;
                }
            }
            Some(entry_id) => {
                // ドライブまたはフォルダのノード。
                let Some(peer) = lan
                    .saved
                    .iter()
                    .find(|s| s.device_id == remote.device_id)
                    .cloned()
                else {
                    continue;
                };
                lan.in_flight.insert(id);
                tree.set_remote_state(id, LoadState::Loading, false);
                spawn_fetch_children(id, peer, entry_id, lan.worker_tx.clone(), lan.proxy.clone());
                changed.pending = true;
                started += 1;
            }
        }
    }
}

fn http_agent(read_timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_recv_response(Some(read_timeout))
        .timeout_recv_body(Some(read_timeout))
        .http_status_as_error(false)
        .build();
    config.into()
}

fn spawn_pair_request(
    node: NodeId,
    target: DiscoveredPeer,
    own_device_id: String,
    own_name: String,
    code: String,
    tx: Sender<WorkerEvent>,
    proxy: EventLoopProxy<WinitUserEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("lisa-lan-pair".into())
        .spawn(move || {
            let result = call_pair(&target, &own_device_id, &own_name, &code);
            let _ = tx.send(WorkerEvent::Paired {
                node,
                device_id: target.device_id,
                result,
            });
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        });
}

fn call_pair(
    target: &DiscoveredPeer,
    own_device_id: &str,
    own_name: &str,
    code: &str,
) -> Result<SavedPeer, String> {
    let agent = http_agent(PAIR_TIMEOUT + Duration::from_secs(5));
    let addr = SocketAddr::new(target.addr, target.port);
    let url = format!("http://{addr}/pair");
    let body = serde_json::to_string(&PairRequest {
        device_id: own_device_id.to_string(),
        name: own_name.to_string(),
        code: code.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let mut response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send(&body)
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("ステータス {status}"));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let reply: PairReply = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let reply_name = clean_peer_name(&reply.name);
    Ok(SavedPeer {
        device_id: target.device_id.clone(),
        name: if reply_name.is_empty() {
            clean_peer_name(&target.name)
        } else {
            reply_name
        },
        key: reply.key,
        addr: target.addr.to_string(),
        port: target.port,
        mcp_port: target.mcp_port,
    })
}

fn spawn_fetch_drives(
    node: NodeId,
    peer: SavedPeer,
    tx: Sender<WorkerEvent>,
    proxy: EventLoopProxy<WinitUserEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("lisa-lan-drives".into())
        .spawn(move || {
            let result = fetch_drives(&peer);
            let _ = tx.send(WorkerEvent::Drives {
                node,
                device_id: peer.device_id,
                result,
            });
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        });
}

fn spawn_fetch_children(
    node: NodeId,
    peer: SavedPeer,
    entry_id: i64,
    tx: Sender<WorkerEvent>,
    proxy: EventLoopProxy<WinitUserEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("lisa-lan-list".into())
        .spawn(move || {
            let result = fetch_children(&peer, entry_id);
            let _ = tx.send(WorkerEvent::Children {
                node,
                device_id: peer.device_id,
                result,
            });
            let _ = proxy.send_event(WinitUserEvent::WakeUp);
        });
}

/// `POST /mcp` で `tools/call` を 1 回呼び、`structuredContent` を返す。
/// 失敗は `(401 かどうか, 理由)`。
pub fn call_mcp_tool(
    addr: &str,
    port: u16,
    key: &str,
    tool: &str,
    arguments: Value,
) -> Result<Value, (bool, String)> {
    let agent = http_agent(Duration::from_secs(30));
    let host = if addr.contains(':') && !addr.starts_with('[') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    };
    let url = format!("http://{host}/mcp");
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
        }
    });
    let body = serde_json::to_string(&payload).map_err(|e| (false, e.to_string()))?;
    let mut response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {key}"))
        .send(&body)
        .map_err(|e| (false, e.to_string()))?;
    let status = response.status().as_u16();
    if status == 401 {
        return Err((true, "認証が通りませんでした（401）".into()));
    }
    if status != 200 {
        return Err((false, format!("HTTP {status}")));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| (false, e.to_string()))?;
    let reply: Value = serde_json::from_str(&text).map_err(|e| (false, e.to_string()))?;
    if let Some(err) = reply.get("error") {
        return Err((false, err.to_string()));
    }
    let result = reply
        .get("result")
        .ok_or_else(|| (false, "result がありません".to_string()))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let msg = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("エラー");
        return Err((false, msg.to_string()));
    }
    result
        .get("structuredContent")
        .cloned()
        .ok_or_else(|| (false, "structuredContent がありません".to_string()))
}

fn fetch_drives(peer: &SavedPeer) -> Result<Vec<RemoteDrive>, (bool, String)> {
    let content = call_mcp_tool(&peer.addr, peer.port, &peer.key, "lisa_drives", json!({}))?;
    let drives: Vec<Drive> =
        serde_json::from_value(content.get("drives").cloned().unwrap_or_else(|| json!([])))
            .map_err(|e| (false, e.to_string()))?;
    let mut out = Vec::new();
    for drive in drives {
        let label =
            lisa_core::display_path::drive_label(&drive.label, drive.mount.as_deref(), &drive.id);
        if let Some(root_id) = drive.root_id {
            out.push(RemoteDrive {
                id: Some(drive.id.clone()),
                mount: drive.mount.clone(),
                label,
                entry_id: root_id,
                listed: true,
                has_hash: drive.root_hash.is_some(),
                has_error: false,
            });
        } else if let Some(mount) = &drive.mount {
            // 相手のサーバーが root_id を返さない場合は、lisa_list に path を渡して item.id を求める。
            let list = call_mcp_tool(
                &peer.addr,
                peer.port,
                &peer.key,
                "lisa_list",
                json!({"path": mount, "limit": 1}),
            )?;
            let item: Item = serde_json::from_value(
                list.get("item")
                    .cloned()
                    .ok_or_else(|| (false, "item がありません".to_string()))?,
            )
            .map_err(|e| (false, e.to_string()))?;
            out.push(RemoteDrive {
                id: Some(drive.id.clone()),
                mount: drive.mount.clone(),
                label,
                entry_id: item.id,
                listed: item.listed,
                has_hash: item.hash.is_some(),
                has_error: item.error.is_some(),
            });
        }
    }
    Ok(out)
}

pub(crate) fn fetch_children(peer: &SavedPeer, entry_id: i64) -> Result<Vec<Item>, (bool, String)> {
    let mut all = Vec::new();
    let mut offset = 0usize;
    loop {
        let content = call_mcp_tool(
            &peer.addr,
            peer.port,
            &peer.key,
            "lisa_list",
            json!({
                "id": entry_id,
                "limit": LIST_LIMIT,
                "offset": offset,
            }),
        )?;
        let total = content
            .get("children_total")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let page: Vec<Item> = serde_json::from_value(
            content
                .get("children")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(|e| (false, e.to_string()))?;
        let count = page.len();
        all.extend(page);
        offset += count;
        if count == 0 || offset >= total {
            break;
        }
    }
    Ok(all)
}

/// 相手のフォルダ（または端末）の合計（下まで全部）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteTotals {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
}

/// 相手のフォルダ（またはドライブ）`entry_id` の合計を、`lisa_info`（`totals: true`）で取る。
/// フォルダでない・まだ読んでいない・エラーがあるときは `Ok(None)`。
pub(crate) fn fetch_folder_total(
    peer: &SavedPeer,
    entry_id: i64,
) -> Result<Option<RemoteTotals>, (bool, String)> {
    let content = call_mcp_tool(
        &peer.addr,
        peer.port,
        &peer.key,
        "lisa_info",
        json!({
            "id": entry_id,
            "totals": true,
        }),
    )?;
    Ok(parse_info_totals(&content))
}

/// `lisa_info` の `structuredContent` からフォルダの合計を取り出す。
fn parse_info_totals(content: &Value) -> Option<RemoteTotals> {
    let item = content.get("item")?;
    if item.get("kind").and_then(Value::as_str) != Some("dir") {
        return None;
    }
    if item.get("listed").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if item.get("error").is_some_and(|e| !e.is_null()) {
        return None;
    }
    let totals = content.get("totals")?;
    let files = totals.get("files").and_then(Value::as_u64)?;
    let folders = totals.get("folders").and_then(Value::as_u64)?;
    let bytes = totals.get("bytes").and_then(Value::as_u64)?;
    Some(RemoteTotals {
        files,
        folders,
        bytes,
    })
}

/// 相手の端末の合計を、ドライブごとに `lisa_info`（`totals: true`）を呼んで足す。
/// `drive_entry_ids` が空なら、先に `lisa_drives` でドライブ一覧を取る。
/// 1 つでも通信に失敗したら `Err`、どのドライブもまだ読んでいなければ `Ok(None)`。
pub(crate) fn fetch_device_total(
    peer: &SavedPeer,
    drive_entry_ids: &[i64],
) -> Result<Option<RemoteTotals>, (bool, String)> {
    let ids: Vec<i64> = if drive_entry_ids.is_empty() {
        fetch_drives(peer)?
            .into_iter()
            .map(|d| d.entry_id)
            .collect()
    } else {
        drive_entry_ids.to_vec()
    };
    let mut total = RemoteTotals {
        files: 0,
        folders: 0,
        bytes: 0,
    };
    let mut any = false;
    for entry_id in ids {
        if let Some(t) = fetch_folder_total(peer, entry_id)? {
            total.files = total.files.saturating_add(t.files);
            total.folders = total.folders.saturating_add(t.folders);
            total.bytes = total.bytes.saturating_add(t.bytes);
            any = true;
        }
    }
    Ok(any.then_some(total))
}

// --- UI（ペアリングのポップアップ、頼んでいる番号の表示、設定の「つないだ端末」一覧） ---

/// ペアリングのバナーの上でクリックしたときに、裏の木が反応しないように見分ける印。
#[derive(Component)]
pub(crate) struct PairBanner;

#[derive(Component)]
struct IncomingPairBanner;

#[derive(Component)]
struct IncomingPairText;

#[derive(Component)]
struct OutgoingPairBanner;

#[derive(Component)]
struct OutgoingPairText;

#[derive(Component)]
struct ConnectedPeersSection;

#[derive(Component)]
struct ConnectedPeersList;

fn spawn_lan_ui(
    mut commands: Commands,
    font: Res<LabelFont>,
    body: Single<Entity, With<PopupBody>>,
) {
    let text_font = TextFont::from_font_size(14.0).with_font(font.0.clone());
    let small_font = TextFont::from_font_size(12.0).with_font(font.0.clone());

    // 1 & 2. 上部のペアリング案内（頼まれたときと、こちらから頼んでいるときを縦に並べて重ならないようにする）。
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(BAR_PX + 12.0),
                left: Val::Px(12.0),
                right: Val::Px(12.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::FlexStart,
                row_gap: Val::Px(8.0),
                ..default()
            },
            GlobalZIndex(15),
            Pickable::IGNORE,
        ))
        .with_children(|stack| {
            // 1. 相手からペアリングを頼まれたときの上部ポップアップ。
            stack
                .spawn((
                    PairBanner,
                    IncomingPairBanner,
                    bevy::picking::hover::Hovered::default(),
                    Node {
                        padding: UiRect::axes(Val::Px(14.0), Val::Px(10.0)),
                        column_gap: Val::Px(12.0),
                        row_gap: Val::Px(6.0),
                        flex_wrap: FlexWrap::Wrap,
                        align_items: AlignItems::Center,
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        display: Display::None,
                        ..default()
                    },
                    BackgroundColor(PANEL_COLOR),
                ))
                .observe(|mut click: On<PointerClick>| click.propagate(false))
                .with_children(|banner| {
                    banner.spawn((
                        IncomingPairText,
                        Text::new(""),
                        text_font.clone(),
                        TextColor(TEXT_COLOR),
                    ));
                    banner
                        .spawn((
                            PairBanner,
                            bevy::picking::hover::Hovered::default(),
                            Node {
                                padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(ALLOW_COLOR),
                            children![(
                                Text::new("許可"),
                                text_font.clone(),
                                TextColor(ALLOW_TEXT_COLOR)
                            )],
                        ))
                        .observe(
                            |mut click: On<PointerClick>,
                             lan: Option<ResMut<LanState>>,
                             mut tree: ResMut<FileTree>,
                             mut changed: ResMut<TreeChanged>| {
                                click.propagate(false);
                                let Some(mut lan) = lan else {
                                    return;
                                };
                                let lan = &mut *lan;
                                if let Some(active) = lan.active_pair.take() {
                                    let clean_name = clean_peer_name(&active.request.name);
                                    match add_peer(
                                        &lan.data_dir,
                                        &active.request.device_id,
                                        &clean_name,
                                    ) {
                                        Ok(peer) => {
                                            let _ = active.reply.send(Some(PairReply {
                                                key: peer.key,
                                                device_id: lan.this_device_id.clone(),
                                                name: lan.this_device_name.clone(),
                                            }));
                                            lan.authorized_peers = load_clean_peers(&lan.data_dir);
                                            lan.authorized_dirty = true;
                                            // 双方から同時に開いて相手の端末ノードが Failed になっていたら、
                                            // もう一度試せるように Unloaded に戻す。
                                            if let Some(dev_node) = find_remote_device_node(
                                                &tree,
                                                &active.request.device_id,
                                            ) && tree.nodes[dev_node].state == LoadState::Failed
                                                && !lan.in_flight.contains(&dev_node)
                                            {
                                                tree.set_remote_state(
                                                    dev_node,
                                                    LoadState::Unloaded,
                                                    false,
                                                );
                                                changed.pending = true;
                                            }
                                        }
                                        Err(error) => {
                                            warn!("ペアリングを保存できませんでした: {error}");
                                            let _ = active.reply.send(None);
                                        }
                                    }
                                }
                            },
                        );
                    banner
                        .spawn((
                            PairBanner,
                            bevy::picking::hover::Hovered::default(),
                            Node {
                                padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(REJECT_COLOR),
                            children![(
                                Text::new("断る"),
                                text_font.clone(),
                                TextColor(REJECT_TEXT_COLOR)
                            )],
                        ))
                        .observe(
                            |mut click: On<PointerClick>, lan: Option<ResMut<LanState>>| {
                                click.propagate(false);
                                let Some(mut lan) = lan else {
                                    return;
                                };
                                if let Some(active) = lan.active_pair.take() {
                                    let _ = active.reply.send(None);
                                }
                            },
                        );
                });

            // 2. こちらからペアリングを頼んでいるときの上部案内。
            stack
                .spawn((
                    OutgoingPairBanner,
                    Node {
                        padding: UiRect::axes(Val::Px(14.0), Val::Px(8.0)),
                        border_radius: BorderRadius::all(Val::Px(6.0)),
                        display: Display::None,
                        ..default()
                    },
                    BackgroundColor(PANEL_COLOR),
                    Pickable::IGNORE,
                ))
                .with_children(|banner| {
                    banner.spawn((
                        OutgoingPairText,
                        Text::new(""),
                        text_font.clone(),
                        TextColor(TEXT_COLOR),
                    ));
                });
        });

    // 3. 設定のポップアップの中に「つないだ端末」の欄を足す。
    commands.entity(*body).with_children(|body| {
        body.spawn((
            ConnectedPeersSection,
            Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(6.0),
                margin: UiRect::top(Val::Px(8.0)),
                ..default()
            },
        ))
        .with_children(|section| {
            section.spawn((
                Text::new("つないだ端末"),
                small_font.clone(),
                TextColor(DIM_COLOR),
            ));
            section.spawn((
                ConnectedPeersList,
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(4.0),
                    ..default()
                },
            ));
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn update_lan_ui(
    mut commands: Commands,
    lan: Option<ResMut<LanState>>,
    font: Res<LabelFont>,
    popup: Res<crate::ui::menu::Popup>,
    mut was_popup_open: Local<bool>,
    mut incoming_banner: Single<&mut Node, (With<IncomingPairBanner>, Without<OutgoingPairBanner>)>,
    mut incoming_text: Single<&mut Text, (With<IncomingPairText>, Without<OutgoingPairText>)>,
    mut outgoing_banner: Single<&mut Node, (With<OutgoingPairBanner>, Without<IncomingPairBanner>)>,
    mut outgoing_text: Single<&mut Text, (With<OutgoingPairText>, Without<IncomingPairText>)>,
    peers_list: Single<Entity, With<ConnectedPeersList>>,
) {
    let Some(mut lan) = lan else {
        return;
    };
    let lan = &mut *lan;

    // 相手からのペアリング頼み。
    if let Some(active) = &lan.active_pair {
        incoming_banner.display = Display::Flex;
        let name = clean_peer_name(&active.request.name);
        let code = clean_peer_name(&active.request.code);
        let msg = format!(
            "{name} がつなぎたいと言っています。番号 {code}（許可すると、ファイルの一覧を見る・アプリを起動し直す・更新することができます）",
        );
        if incoming_text.0 != msg {
            incoming_text.0 = msg;
        }
    } else {
        incoming_banner.display = Display::None;
    }

    // こちらから頼んでいる番号の表示。
    if let Some((_, name, code)) = &lan.outgoing_pair {
        outgoing_banner.display = Display::Flex;
        let name = clean_peer_name(name);
        let msg = format!("{name} の画面で 番号 {code} を許可してください");
        if outgoing_text.0 != msg {
            outgoing_text.0 = msg;
        }
    } else {
        outgoing_banner.display = Display::None;
    }

    // 設定ポップアップを開いたときにも `lan.json` を読み直す。
    if popup.open && !*was_popup_open {
        lan.authorized_peers = load_clean_peers(&lan.data_dir);
        lan.authorized_dirty = true;
    }
    *was_popup_open = popup.open;

    if lan.authorized_dirty {
        lan.authorized_dirty = false;
        let list_entity = *peers_list;
        commands.entity(list_entity).despawn_children();
        let text_font = TextFont::from_font_size(13.0).with_font(font.0.clone());
        let peers = lan.authorized_peers.clone();
        commands.entity(list_entity).with_children(|list| {
            if peers.is_empty() {
                list.spawn((
                    Text::new("（まだありません）"),
                    text_font.clone(),
                    TextColor(DIM_COLOR),
                ));
                return;
            }
            for peer in peers {
                let device_id = peer.device_id.clone();
                list.spawn(Node {
                    align_items: AlignItems::Center,
                    column_gap: Val::Px(8.0),
                    ..default()
                })
                .with_children(|row| {
                    row.spawn((
                        Text::new(clean_peer_name(&peer.name)),
                        text_font.clone(),
                        TextColor(TEXT_COLOR),
                        Node {
                            flex_grow: 1.0,
                            ..default()
                        },
                    ));
                    row.spawn((
                        Node {
                            padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(BUTTON_COLOR),
                        children![(
                            Text::new("取り消す"),
                            text_font.clone(),
                            TextColor(TEXT_COLOR)
                        )],
                    ))
                    .observe(
                        move |mut click: On<PointerClick>, lan: Option<ResMut<LanState>>| {
                            click.propagate(false);
                            let Some(mut lan) = lan else {
                                return;
                            };
                            let _ = revoke_peer(&lan.data_dir, &device_id);
                            lan.authorized_peers = load_clean_peers(&lan.data_dir);
                            lan.authorized_dirty = true;
                        },
                    );
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_peers_round_trip_and_two_instance_mcp_flow() {
        let dir_a = std::env::temp_dir().join(format!("lisa-lan-a-{}", std::process::id()));
        let dir_b = std::env::temp_dir().join(format!("lisa-lan-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        // 相手側（A）の DB に端末とドライブ、ファイルを入れておく。
        let sub_dir = dir_a.join("drive");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(sub_dir.join("hello.txt"), b"world").unwrap();
        {
            let mut db = lisa_core::db::Db::open(&dir_a.join("lisa.db")).unwrap();
            let dev = db.this_device("PC-A", "PC-A", "macos").unwrap();
            let vol = lisa_core::volumes::Volume {
                id: Some("drive:a".into()),
                label: "DriveA".into(),
                fs: "apfs".into(),
                mount_path: sub_dir.clone(),
                capacity: Some(1000),
                available: Some(500),
                removable: false,
                network: false,
                read_only: false,
                case_sensitive: Some(false),
                hidden: false,
            };
            db.upsert_drive(&dev, &vol).unwrap();
            let listing = lisa_core::listing::read_dir(&sub_dir).map_err(|e| e.to_string());
            db.save_listing("drive:a", &sub_dir, &sub_dir, &listing)
                .unwrap();
            lisa_core::hash::hash_drive(
                &mut db,
                "drive:a",
                &sub_dir,
                1,
                &mut |_| {},
                &lisa_core::hash::Pace::FREE,
            )
            .unwrap();
        }

        // A の HttpListener を立ち上げ、ペアリングを許可する口を付ける。
        let token = load_or_create_token(&dir_a).unwrap();
        let dir_a_for_pair = dir_a.clone();
        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_a.clone(),
                cache_dir: None,
            },
            token.clone(),
        )
        .unwrap()
        .with_pair_handler(Box::new(move |req| {
            let peer = add_peer(&dir_a_for_pair, &req.device_id, &req.name).ok()?;
            Some(PairReply {
                key: peer.key,
                device_id: "dev-a".into(),
                name: "PC-A".into(),
            })
        }));
        let addr = listener.local_addr();
        let handle = listener.handle();
        let server_thread = std::thread::spawn(move || listener.run());

        // 頼む側（B）から /pair → peers.json 保存 → lisa_drives → lisa_list を確かめる。
        let disc = DiscoveredPeer {
            device_id: "dev-a".into(),
            name: "PC-A".into(),
            addr: addr.ip(),
            port: addr.port(),
            mcp_port: Some(SIDECAR_PORT),
            last_seen: Instant::now(),
        };
        let paired = call_pair(&disc, "dev-b", "PC-B", "4321").unwrap();
        save_saved_peers(&dir_b, std::slice::from_ref(&paired));
        assert_eq!(load_saved_peers(&dir_b), vec![paired.clone()]);

        let drives = fetch_drives(&paired).unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].label, "DriveA");
        let root_entry_id = drives[0].entry_id;

        let children = fetch_children(&paired, root_entry_id).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "hello.txt");
        assert_eq!(children[0].size, 5);

        let call =
            |key: &str, tool: &str, args| call_mcp_tool(&paired.addr, paired.port, key, tool, args);
        let tool = "lisa_duplicate_catalog";
        assert!(
            call(&token, tool, json!({}))
                .unwrap_err()
                .1
                .contains("ペアリング")
        );
        let page: lisa_core::dups::CatalogPage =
            serde_json::from_value(call(&paired.key, tool, json!({})).unwrap()).unwrap();
        assert!(page.complete);
        assert_eq!(page.keys.len(), 1);
        let found: Vec<lisa_core::dups::Locations> = serde_json::from_value(
            call(
                &paired.key,
                "lisa_duplicate_locations",
                json!({"generation": page.generation, "keys": page.keys}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(found[0].entries[0].chain, [root_entry_id, children[0].id]);

        handle.unblock();
        server_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn clean_peer_name_strips_controls_and_truncates() {
        assert_eq!(clean_peer_name("  PC-A\n\r\t1  "), "PC-A 1");
        assert_eq!(clean_peer_name("\n\r\x07"), "");
        let long = "あ".repeat(50);
        let cleaned = clean_peer_name(&long);
        assert_eq!(cleaned.chars().count(), MAX_NAME_CHARS + 1);
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn pair_queue_caps_at_five_replaces_same_device_and_times_from_shown() {
        let t0 = Instant::now();
        let mut active = None;
        let mut queue = VecDeque::new();

        let make = |id: &str, name: &str, code: &str| {
            let (tx, rx) = channel();
            (
                PairRequest {
                    device_id: id.into(),
                    name: name.into(),
                    code: code.into(),
                },
                tx,
                rx,
            )
        };

        // 1 件目が active に入り、続く 5 件が queue に入り、7 件目（queue 6 件目）はその場で断られる。
        let (req0, tx0, rx0) = make("dev-0", "PC-0", "0000");
        let (req1, tx1, rx1) = make("dev-1", "PC-1", "1111");
        let (req2, tx2, _rx2) = make("dev-2", "PC-2", "2222");
        let (req3, tx3, _rx3) = make("dev-3", "PC-3", "3333");
        let (req4, tx4, _rx4) = make("dev-4", "PC-4", "4444");
        let (req5, tx5, _rx5) = make("dev-5", "PC-5", "5555");
        let (req6, tx6, rx6) = make("dev-6", "PC-6", "6666");

        step_pair_queue(&mut active, &mut queue, [(req0, tx0)], t0);
        assert_eq!(active.as_ref().unwrap().request.device_id, "dev-0");
        step_pair_queue(
            &mut active,
            &mut queue,
            [
                (req1, tx1),
                (req2, tx2),
                (req3, tx3),
                (req4, tx4),
                (req5, tx5),
                (req6, tx6),
            ],
            t0,
        );
        assert_eq!(queue.len(), MAX_PAIR_QUEUE);
        // 上限を超えた dev-6 はすぐ None が返る。
        assert_eq!(rx6.try_recv().unwrap(), None);

        // 同じ device_id（dev-1）の新しい頼みが来たら、古い頼みは None が返って新しいものに置き換わる。
        let (req1_new, tx1_new, _rx1_new) = make("dev-1", "PC-1-new", "9999");
        step_pair_queue(&mut active, &mut queue, [(req1_new, tx1_new)], t0);
        assert_eq!(rx1.try_recv().unwrap(), None);
        assert_eq!(queue.len(), MAX_PAIR_QUEUE);
        assert!(
            queue
                .iter()
                .any(|p| p.request.device_id == "dev-1" && p.request.code == "9999")
        );

        // 60 秒が過ぎて dev-0 が期限切れになっても、列にいた dev-2 はその時点から 60 秒数えるので消えない。
        let t1 = t0 + PAIR_TIMEOUT + Duration::from_secs(1);
        step_pair_queue(&mut active, &mut queue, [], t1);
        assert_eq!(rx0.try_recv().unwrap(), None);
        assert_eq!(active.as_ref().unwrap().request.device_id, "dev-2");
        assert_eq!(active.as_ref().unwrap().shown_at, Some(t1));
    }

    #[test]
    fn unauthorized_resets_remote_device_node_for_retry() {
        let mut tree = FileTree::dummy();
        let dev_node = tree.ensure_remote_device("dev-remote", "PC-R", LoadState::Unloaded);
        tree.add_remote_drives(
            dev_node,
            vec![RemoteDrive {
                id: None,
                mount: None,
                label: "C:".into(),
                entry_id: 10,
                listed: true,
                has_hash: false,
                has_error: false,
            }],
        );
        assert!(tree.nodes[dev_node].loaded);
        assert_eq!(tree.nodes[dev_node].children.len(), 1);

        let mut in_flight = HashSet::new();
        in_flight.insert(dev_node);
        reset_unauthorized_device(&mut tree, &mut in_flight, "dev-remote");

        assert!(!in_flight.contains(&dev_node));
        assert!(!tree.nodes[dev_node].loaded);
        assert!(tree.nodes[dev_node].children.is_empty());
        assert_eq!(tree.nodes[dev_node].state, LoadState::Failed);

        // もう一度開いたとき（Unloaded に戻したあと）にドライブを再追加できることを確かめる。
        tree.set_remote_state(dev_node, LoadState::Unloaded, false);
        tree.add_remote_drives(
            dev_node,
            vec![RemoteDrive {
                id: None,
                mount: None,
                label: "D:".into(),
                entry_id: 20,
                listed: true,
                has_hash: true,
                has_error: false,
            }],
        );
        assert!(tree.nodes[dev_node].loaded);
        assert_eq!(tree.nodes[dev_node].children.len(), 1);
    }

    #[test]
    fn fetch_remote_folder_and_device_totals_via_mcp() {
        let dir_a = std::env::temp_dir().join(format!("lisa-lan-totals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_a);
        let drive1 = dir_a.join("drive1");
        let nested = drive1.join("sub").join("deep");
        let unlisted = drive1.join("unlisted");
        let drive2 = dir_a.join("drive2");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&unlisted).unwrap();
        std::fs::create_dir_all(&drive2).unwrap();
        std::fs::write(drive1.join("top.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(drive1.join("sub").join("mid.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(nested.join("leaf.bin"), vec![0u8; 300]).unwrap();
        std::fs::write(drive2.join("d2.bin"), vec![0u8; 400]).unwrap();

        let (sub_id, unlisted_id, d1_root, d2_root) = {
            let mut db = lisa_core::db::Db::open(&dir_a.join("lisa.db")).unwrap();
            let dev = db.this_device("PC-A", "PC-A", "macos").unwrap();
            let make_vol = |id: &str, label: &str, mount: &Path| lisa_core::volumes::Volume {
                id: Some(id.into()),
                label: label.into(),
                fs: "apfs".into(),
                mount_path: mount.to_path_buf(),
                capacity: None,
                available: None,
                removable: false,
                network: false,
                read_only: false,
                case_sensitive: Some(false),
                hidden: false,
            };
            db.upsert_drive(&dev, &make_vol("drive:1", "D1", &drive1))
                .unwrap();
            db.upsert_drive(&dev, &make_vol("drive:2", "D2", &drive2))
                .unwrap();
            for (did, mount, folder) in [
                ("drive:1", &drive1, drive1.as_path()),
                ("drive:1", &drive1, drive1.join("sub").as_path()),
                ("drive:1", &drive1, nested.as_path()),
                ("drive:2", &drive2, drive2.as_path()),
            ] {
                let listing = lisa_core::listing::read_dir(folder).map_err(|e| e.to_string());
                db.save_listing(did, mount, folder, &listing).unwrap();
            }
            let d1_root = db.drive_root("drive:1").unwrap().unwrap().id;
            let d2_root = db.drive_root("drive:2").unwrap().unwrap().id;
            let sub_id = *lisa_core::query::locate(&db, &drive1.join("sub"))
                .unwrap()
                .unwrap()
                .chain
                .last()
                .unwrap();
            let unlisted_id = *lisa_core::query::locate(&db, &unlisted)
                .unwrap()
                .unwrap()
                .chain
                .last()
                .unwrap();
            (sub_id, unlisted_id, d1_root, d2_root)
        };

        let token = load_or_create_token(&dir_a).unwrap();
        let peer_rec = add_peer(&dir_a, "dev-b", "PC-B").unwrap();
        let listener = HttpListener::bind(
            "127.0.0.1:0",
            Server {
                data_dir: dir_a.clone(),
                cache_dir: None,
            },
            token,
        )
        .unwrap();
        let addr = listener.local_addr();
        let handle = listener.handle();
        let server_thread = std::thread::spawn(move || listener.run());

        let saved = SavedPeer {
            device_id: "dev-a".into(),
            name: "PC-A".into(),
            key: peer_rec.key,
            addr: addr.ip().to_string(),
            port: addr.port(),
            mcp_port: None,
        };

        // 途中のフォルダ: mid.bin (200) + deep/leaf.bin (300) = 2 ファイル、1 フォルダ（deep）、500 バイト。
        assert_eq!(
            fetch_folder_total(&saved, sub_id).unwrap(),
            Some(RemoteTotals {
                files: 2,
                folders: 1,
                bytes: 500,
            })
        );
        // まだ読んでいないフォルダは None。
        assert_eq!(fetch_folder_total(&saved, unlisted_id).unwrap(), None);
        // ドライブ 1: top + mid + leaf (3 ファイル)、sub + deep + unlisted (3 フォルダ)、600 バイト。
        assert_eq!(
            fetch_folder_total(&saved, d1_root).unwrap(),
            Some(RemoteTotals {
                files: 3,
                folders: 3,
                bytes: 600,
            })
        );
        // 端末全体（ドライブ一覧を明示しても、空で自動取得しても同じ）: 4 ファイル、3 フォルダ、1000 バイト。
        let expected_dev = Some(RemoteTotals {
            files: 4,
            folders: 3,
            bytes: 1000,
        });
        assert_eq!(
            fetch_device_total(&saved, &[d1_root, d2_root]).unwrap(),
            expected_dev
        );
        assert_eq!(fetch_device_total(&saved, &[]).unwrap(), expected_dev);

        handle.unblock();
        server_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir_a);
    }
}
