//! ファイルの縮小画像。原本には書き込まず、専用の保存先だけを扱う。

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime};

pub(super) use lisa_core::remote::FilePreviewScope as Scope;
use lisa_core::remote::{self, Command, Reply};
use serde::{Deserialize, Serialize};

use super::{LoadedPreview, PreviewKey};

const LIMIT: u64 = 1 << 30;
const LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_HEADER: usize = 64 * 1024;
const MAX_WEBP: usize = 512 * 1024;
// 一覧をまとめて開いたときの生成を吸収する。RGBA の上限では約 16 MiB。
const SAVE_QUEUE: usize = 64;
const MAGIC: [u8; 4] = *b"LFP1";

#[derive(Serialize, Deserialize)]
struct Header {
    cartridge: String,
    size: u64,
    #[serde(with = "timestamp")]
    mtime: SystemTime,
    width: u32,
    height: u32,
    compressed: usize,
    ancestors: Vec<String>,
}

// serde の SystemTime は Unix epoch より前を扱えないため、符号付きの差を保存する。
mod timestamp {
    use super::*;
    use serde::de::Error;

    pub fn serialize<S: serde::Serializer>(time: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let (before, delta) = match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(delta) => (false, delta),
            Err(error) => (true, error.duration()),
        };
        (before, delta).serialize(s)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let (before, delta): (bool, Duration) = Deserialize::deserialize(d)?;
        if before {
            SystemTime::UNIX_EPOCH.checked_sub(delta)
        } else {
            SystemTime::UNIX_EPOCH.checked_add(delta)
        }
        .ok_or_else(|| D::Error::custom("範囲外の更新日時"))
    }
}

#[cfg(windows)]
fn native_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(unix)]
fn native_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(windows)]
fn native_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let (wide, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let wide: Vec<_> = wide.iter().copied().map(u16::from_le_bytes).collect();
    Some(std::ffi::OsString::from_wide(&wide).into())
}

#[cfg(unix)]
fn native_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(std::ffi::OsString::from_vec(bytes).into())
}

fn digest(path: &Path) -> String {
    let mut hash = blake3::Hasher::new();
    let bytes = native_bytes(path);
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes.as_ref());
    hash.finalize().to_hex().to_string()
}

pub(super) fn supported(id: &str) -> bool {
    matches!(id, "image" | "video" | "pdf")
}

pub(super) fn contains(path: &Path, scope: Scope, candidate: &Path) -> bool {
    candidate == path
        || match scope {
            Scope::File => false,
            Scope::Children => candidate.parent() == Some(path),
            Scope::Descendants => candidate.starts_with(path),
        }
}

enum Work {
    Save(PreviewKey, Arc<LoadedPreview>, Arc<AtomicBool>),
    Remove(u64, PathBuf, Scope, Sender<bool>),
}

pub(super) struct Disk {
    dir: PathBuf,
    control_path: Mutex<Option<PathBuf>>,
    work: Sender<Work>,
    queued: Arc<AtomicUsize>,
    // 失効中と失敗後は、該当する記録を読まない。再更新で成功すれば解除する。
    removing: Arc<Mutex<Vec<(u64, PathBuf, Scope)>>>,
    remove_ticket: AtomicU64,
    tokens: Mutex<Vec<(PathBuf, Weak<AtomicBool>)>>,
    notices: Mutex<Receiver<(PathBuf, Scope)>>,
}

impl Drop for Disk {
    fn drop(&mut self) {
        if let Ok(path) = self.control_path.get_mut()
            && let Some(path) = path.take()
        {
            remote::remove(&path);
        }
    }
}

impl Disk {
    pub(super) fn open(dir: PathBuf) -> Option<Arc<Self>> {
        fs::create_dir_all(&dir).ok()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("owner.lock"))
            .ok()?;
        lock.try_lock().ok()?;
        let (work, rx) = channel();
        let (notice, notices) = channel();
        let disk = Arc::new(Self {
            dir: dir.clone(),
            control_path: Mutex::new(None),
            work,
            queued: Arc::default(),
            removing: Arc::default(),
            remove_ticket: AtomicU64::new(0),
            tokens: Mutex::default(),
            notices: Mutex::new(notices),
        });
        let queued = disk.queued.clone();
        let removing = disk.removing.clone();
        std::thread::Builder::new()
            .name("lisa-file-previews".into())
            .spawn(move || {
                lisa_core::pace::set_thread_background(true);
                let _lock = lock;
                let mut store = Store {
                    dir,
                    limit: LIMIT,
                    bytes: 0,
                    sequence: 0,
                    warned_write: false,
                };
                store.sweep();
                for command in rx {
                    match command {
                        Work::Save(key, image, valid) => {
                            if store.bytes <= store.limit && valid.load(Ordering::Acquire) {
                                store.save(&key, &image, &valid);
                                if store.bytes > store.limit {
                                    store.sweep();
                                }
                            }
                            queued.fetch_sub(1, Ordering::Relaxed);
                        }
                        Work::Remove(ticket, path, scope, reply) => {
                            let ok = store.remove(&path, scope).is_ok();
                            if ok {
                                let mut pending = removing.lock().expect("ファイル画像の失効");
                                pending.retain(|(t, p, s)| {
                                    !(*t <= ticket
                                        && *p == path
                                        && std::mem::discriminant(s)
                                            == std::mem::discriminant(&scope))
                                });
                            }
                            let _ = reply.send(ok);
                        }
                    }
                }
            })
            .ok()?;
        let weak = Arc::downgrade(&disk);
        if let Ok(control_path) = remote::listen(&disk.dir, move |incoming| {
            let ok = if let Command::InvalidateFilePreviews { path, scope } = incoming.command
                && let Some(path) = native_path(path).filter(|path| path.is_absolute())
                && let Some(disk) = weak.upgrade()
            {
                let done = disk.remove(path.clone(), scope);
                let _ = notice.send((path, scope));
                done.recv().unwrap_or(false)
            } else {
                false
            };
            incoming
                .reply
                .send(Reply {
                    ok,
                    error: (!ok).then(|| "ファイル画像のディスク更新が未完了です".into()),
                    ..Reply::default()
                })
                .is_ok()
        }) {
            *disk.control_path.lock().expect("ファイル画像の制御口") = Some(control_path);
        }
        Some(disk)
    }

    pub(super) fn track(&self, key: &PreviewKey, valid: &Arc<AtomicBool>) {
        let mut tokens = self.tokens.lock().expect("ファイル画像の世代");
        if tokens.len() >= 1024 {
            tokens.retain(|(_, valid)| valid.strong_count() > 0);
        }
        tokens.push((key.path.clone(), Arc::downgrade(valid)));
    }

    pub(super) fn notices(&self) -> Vec<(PathBuf, Scope)> {
        self.notices
            .lock()
            .expect("ファイル画像の通知")
            .try_iter()
            .collect()
    }

    pub(super) fn load(&self, key: &PreviewKey) -> Option<LoadedPreview> {
        if !key.path.is_absolute()
            || self
                .removing
                .lock()
                .ok()?
                .iter()
                .any(|(_, path, scope)| contains(path, *scope, &key.path))
        {
            return None;
        }
        let mtime = key.mtime?;
        let path = record_path(&self.dir, &key.path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .or_else(|_| File::open(path))
            .ok()?;
        let modified = file.metadata().ok()?.modified().ok()?;
        if SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age > LIFETIME)
        {
            return None;
        }
        let header = read_header(&mut file)?;
        if header.cartridge != key.cartridge || header.size != key.len || header.mtime != mtime {
            return None;
        }
        let mut data = Vec::with_capacity(header.compressed);
        (&mut file)
            .take(header.compressed as u64 + 1)
            .read_to_end(&mut data)
            .ok()?;
        if data.len() != header.compressed {
            return None;
        }
        let features = webp::BitstreamFeatures::new(&data)?;
        let (width, height) = (header.width, header.height);
        if features.width() != width
            || features.height() != height
            || features.has_animation()
            || !matches!(features.format(), Some(webp::BitstreamFormat::Lossless))
        {
            return None;
        }
        let image = webp::Decoder::new(&data).decode()?;
        let pixels = (width * height) as usize;
        let rgba = if features.has_alpha() && image.len() == pixels * 4 {
            image.to_vec()
        } else if !features.has_alpha() && image.len() == pixels * 3 {
            let (chunks, remainder) = image.as_chunks::<3>();
            if !remainder.is_empty() {
                return None;
            }
            chunks
                .iter()
                .flat_map(|&[r, g, b]| [r, g, b, 255])
                .collect()
        } else {
            return None;
        };
        let _ = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
        Some(LoadedPreview::Image {
            width,
            height,
            rgba,
        })
    }

    pub(super) fn save(&self, key: PreviewKey, image: Arc<LoadedPreview>, valid: Arc<AtomicBool>) {
        if self.queued.fetch_add(1, Ordering::Relaxed) >= SAVE_QUEUE
            || self.work.send(Work::Save(key, image, valid)).is_err()
        {
            self.queued.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub(super) fn remove(&self, path: PathBuf, scope: Scope) -> Receiver<bool> {
        let (tx, rx) = channel();
        let ticket = self
            .remove_ticket
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut tokens = self.tokens.lock().expect("ファイル画像の世代");
        let mut pending = self.removing.lock().expect("ファイル画像の失効");
        pending.push((ticket, path.clone(), scope));
        tokens.retain(|(candidate, valid)| {
            let Some(valid) = valid.upgrade() else {
                return false;
            };
            if contains(&path, scope, candidate) {
                valid.store(false, Ordering::Release);
                return false;
            }
            true
        });
        let _ = self.work.send(Work::Remove(ticket, path, scope, tx));
        rx
    }
}

type OwnerRemoval = (PathBuf, Vec<u8>, Scope, Sender<bool>);

pub(super) fn remove_from_owner(dir: PathBuf, path: &Path, scope: Scope) -> Receiver<bool> {
    static SENDER: OnceLock<Sender<OwnerRemoval>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = channel::<OwnerRemoval>();
        let _ = std::thread::Builder::new()
            .name("lisa-file-previews-ipc".into())
            .spawn(move || {
                for (dir, path, scope, reply) in rx {
                    let command = Command::InvalidateFilePreviews { path, scope };
                    let ok = remote::send(&dir, command).is_ok_and(|reply| reply.ok);
                    let _ = reply.send(ok);
                }
            });
        tx
    });
    let (tx, rx) = channel();
    if sender
        .send((dir, native_bytes(path).to_vec(), scope, tx))
        .is_err()
    {
        let (fail_tx, fail_rx) = channel();
        let _ = fail_tx.send(false);
        return fail_rx;
    }
    rx
}

fn record_path(dir: &Path, path: &Path) -> PathBuf {
    dir.join(format!("{}.preview", digest(path)))
}

fn dimensions(width: u32, height: u32) -> bool {
    (1..=256).contains(&width) && (1..=256).contains(&height)
}

fn read_header(file: &mut File) -> Option<Header> {
    let mut prefix = [0; 8];
    file.read_exact(&mut prefix).ok()?;
    if prefix[..4] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(prefix[4..].try_into().ok()?) as usize;
    if len > MAX_HEADER {
        return None;
    }
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes).ok()?;
    let header: Header = serde_json::from_slice(&bytes).ok()?;
    (dimensions(header.width, header.height) && (1..=MAX_WEBP).contains(&header.compressed))
        .then_some(header)
}

struct Store {
    dir: PathBuf,
    limit: u64,
    bytes: u64,
    sequence: u64,
    warned_write: bool,
}

fn known_record(name: &str) -> bool {
    name.strip_suffix(".preview").is_some_and(|stem| {
        stem.len() == 64
            && stem
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

fn known_temp(name: &str) -> bool {
    let Some((pid, sequence)) = name
        .strip_prefix(".save-")
        .and_then(|s| s.strip_suffix(".tmp"))
        .and_then(|s| s.split_once('-'))
    else {
        return false;
    };
    pid.parse::<u32>().is_ok() && sequence.parse::<u64>().is_ok()
}

impl Store {
    fn save(&mut self, key: &PreviewKey, preview: &LoadedPreview, valid: &AtomicBool) {
        if !key.path.is_absolute() || !supported(key.cartridge) {
            return;
        }
        let Some(mtime) = key.mtime else { return };
        let LoadedPreview::Image {
            width,
            height,
            rgba,
        } = preview
        else {
            return;
        };
        if !dimensions(*width, *height) || rgba.len() != (*width * *height * 4) as usize {
            return;
        }
        let Ok(mut config) = webp::WebPConfig::new() else {
            return;
        };
        // 保存より表示を優先する。圧縮に時間をかけず、画素は可逆のまま。
        config.method = 0;
        config.lossless = 1;
        config.exact = 1;
        let Ok(encoded) = webp::Encoder::from_rgba(rgba, *width, *height).encode_advanced(&config)
        else {
            return;
        };
        let header = Header {
            cartridge: key.cartridge.into(),
            size: key.len,
            mtime,
            width: *width,
            height: *height,
            compressed: encoded.len(),
            ancestors: key.path.ancestors().skip(1).map(digest).collect(),
        };
        let Ok(header) = serde_json::to_vec(&header) else {
            return;
        };
        if header.len() > MAX_HEADER || encoded.len() > MAX_WEBP {
            return;
        }
        let size = (8 + header.len() + encoded.len()) as u64;
        if size > self.limit || !valid.load(Ordering::Acquire) {
            return;
        }
        self.sequence = self.sequence.wrapping_add(1);
        let temp = self.dir.join(format!(
            ".save-{}-{}.tmp",
            std::process::id(),
            self.sequence
        ));
        let path = record_path(&self.dir, &key.path);
        let old = fs::metadata(&path).map_or(0, |meta| meta.len());
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            Err(error) => {
                self.warn_write_once(&error);
                return;
            }
        };
        let written = file
            .write_all(&MAGIC)
            .and_then(|()| file.write_all(&(header.len() as u32).to_le_bytes()))
            .and_then(|()| file.write_all(&header))
            .and_then(|()| file.write_all(&encoded));
        drop(file);
        match written {
            Ok(()) if valid.load(Ordering::Acquire) => match fs::rename(&temp, path) {
                Ok(()) => self.bytes = self.bytes.saturating_sub(old) + size,
                Err(error) => {
                    self.warn_write_once(&error);
                    let _ = fs::remove_file(temp);
                }
            },
            Ok(()) => {
                let _ = fs::remove_file(temp);
            }
            Err(error) => {
                self.warn_write_once(&error);
                let _ = fs::remove_file(temp);
            }
        }
    }

    fn warn_write_once(&mut self, error: &io::Error) {
        if !self.warned_write {
            self.warned_write = true;
            bevy::log::warn!("ファイル画像のディスク保存に失敗しました: {error}");
        }
    }

    fn delete(&mut self, path: &Path, size: u64) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => self.bytes = self.bytes.saturating_sub(size),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn remove(&mut self, path: &Path, scope: Scope) -> io::Result<()> {
        let mut result = Ok(());
        let own = record_path(&self.dir, path);
        if let Err(error) = self.delete(&own, fs::metadata(&own).map_or(0, |meta| meta.len())) {
            result = Err(error);
        }
        if matches!(scope, Scope::File) {
            return result;
        }
        let ancestor = digest(path);
        for entry in fs::read_dir(&self.dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        result = Err(error);
                    }
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        result = Err(error);
                    }
                    continue;
                }
            };
            if !file_type.is_file() || !known_record(&entry.file_name().to_string_lossy()) {
                continue;
            }
            let entry = entry.path();
            let mut file = match File::open(&entry) {
                Ok(file) => file,
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        result = Err(error);
                    }
                    continue;
                }
            };
            let found = read_header(&mut file).is_some_and(|header| match scope {
                Scope::Children => header.ancestors.first() == Some(&ancestor),
                _ => header.ancestors.contains(&ancestor),
            });
            let size = match file.metadata() {
                Ok(meta) => meta.len(),
                Err(error) => {
                    if error.kind() != io::ErrorKind::NotFound {
                        result = Err(error);
                    }
                    continue;
                }
            };
            drop(file);
            if found && let Err(error) = self.delete(&entry, size) {
                result = Err(error);
            }
        }
        result
    }

    fn sweep(&mut self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        let mut records = Vec::new();
        self.bytes = 0;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if known_temp(&name) {
                let _ = fs::remove_file(entry.path());
            } else if known_record(&name)
                && let Ok(meta) = entry.metadata()
            {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                self.bytes += meta.len();
                records.push((modified, entry.path(), meta.len()));
            }
        }
        records.sort_unstable();
        for (modified, path, size) in records {
            if self.bytes > self.limit
                || now.duration_since(modified).is_ok_and(|age| age > LIFETIME)
            {
                let _ = self.delete(&path, size);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    pub(crate) struct Temp(pub PathBuf);

    impl Temp {
        pub(crate) fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "lisa-file-previews-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(path: PathBuf) -> PreviewKey {
        PreviewKey {
            path,
            len: 21,
            cartridge: "image",
            mtime: Some(SystemTime::UNIX_EPOCH - Duration::new(7, 12345)),
        }
    }

    fn fixture() -> LoadedPreview {
        LoadedPreview::Image {
            width: 2,
            height: 1,
            rgba: vec![72, 11, 90, 0, 255, 9, 12, 128],
        }
    }

    fn store(dir: PathBuf) -> Store {
        Store {
            dir,
            limit: LIMIT,
            bytes: 0,
            sequence: 0,
            warned_write: false,
        }
    }

    #[test]
    fn レコードは全精度の属性と透明ピクセルを照合する() {
        let tmp = Temp::new();
        let key = key(tmp.0.join("Photo.png"));
        let mut store = store(tmp.0.clone());
        let image = fixture();
        store.save(&key, &image, &AtomicBool::new(true));
        let disk = Disk::open(tmp.0.clone()).unwrap();
        let Some(LoadedPreview::Image {
            width,
            height,
            rgba,
        }) = disk.load(&key)
        else {
            panic!("保存した画像が読めない");
        };
        assert_eq!((width, height), (2, 1));
        assert_eq!(rgba, [72, 11, 90, 0, 255, 9, 12, 128]);
        for mismatch in [
            PreviewKey {
                mtime: key.mtime.map(|t| t + Duration::from_nanos(1)),
                ..key.clone()
            },
            PreviewKey {
                len: key.len + 1,
                ..key.clone()
            },
            PreviewKey {
                path: tmp.0.join("photo.png"),
                ..key.clone()
            },
            PreviewKey {
                cartridge: "pdf",
                ..key.clone()
            },
            PreviewKey {
                mtime: None,
                ..key.clone()
            },
        ] {
            assert!(disk.load(&mismatch).is_none());
        }
        let path = record_path(&tmp.0, &key.path);
        fs::write(&path, b"LFP1\xff\xff\xff\xff").unwrap();
        assert!(disk.load(&key).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn 非utf8のパスを区別する() {
        use std::os::unix::ffi::OsStringExt;
        let a = PathBuf::from("/").join(std::ffi::OsString::from_vec(vec![0xfe]));
        let b = PathBuf::from("/").join(std::ffi::OsString::from_vec(vec![0xff]));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        assert_ne!(digest(&a), digest(&b));
        assert_eq!(native_path(native_bytes(&a).to_vec()), Some(a));
    }

    #[test]
    fn 期限と連続保存時の使用順と中断ファイルを掃除する() {
        let tmp = Temp::new();
        let mut store = store(tmp.0.clone());
        let a = key(tmp.0.join("a"));
        let b = key(tmp.0.join("b"));
        let c = key(tmp.0.join("c"));
        let d = key(tmp.0.join("d"));
        let valid = AtomicBool::new(true);
        let touch = |key: &PreviewKey, age| {
            File::options()
                .write(true)
                .open(record_path(&tmp.0, &key.path))
                .unwrap()
                .set_modified(SystemTime::now() - age)
                .unwrap();
        };
        store.save(&a, &fixture(), &valid);
        store.save(&b, &fixture(), &valid);
        store.save(&c, &fixture(), &valid);
        store.limit = store.bytes / 3 * 2;
        touch(&a, LIFETIME + Duration::from_secs(3600));
        touch(&b, Duration::from_secs(3600));
        fs::write(tmp.0.join(".save-123-1.tmp"), b"partial").unwrap();
        fs::write(tmp.0.join("unknown.preview"), b"other").unwrap();
        store.sweep();
        assert!(!record_path(&tmp.0, &a.path).exists());
        assert!(!tmp.0.join(".save-123-1.tmp").exists());
        assert!(tmp.0.join("unknown.preview").exists());
        store.save(&d, &fixture(), &valid);
        assert!(store.bytes > store.limit);
        store.sweep();
        assert!(store.bytes <= store.limit);
        assert!(!record_path(&tmp.0, &b.path).exists());
        assert!(record_path(&tmp.0, &c.path).exists());
        assert!(record_path(&tmp.0, &d.path).exists());
    }

    #[test]
    fn 排他を取れない側の失効は所有側が完了を返す() {
        let tmp = Temp::new();
        let cache = tmp.0.join("cache");
        let disk = Disk::open(cache.clone()).unwrap();
        assert!(Disk::open(cache.clone()).is_none());
        let folder = tmp.0.join("source");
        let direct = key(folder.join("direct"));
        let nested = key(folder.join("child/deep"));
        let unrelated = key(tmp.0.join("elsewhere/picture"));
        let valid = Arc::new(AtomicBool::new(true));
        disk.track(&direct, &valid);
        for key in [&direct, &nested, &unrelated] {
            disk.save(
                key.clone(),
                Arc::new(fixture()),
                Arc::new(AtomicBool::new(true)),
            );
        }
        let timeout = Duration::from_secs(5);
        assert_eq!(
            remove_from_owner(cache.clone(), &folder, Scope::Children).recv_timeout(timeout),
            Ok(true)
        );
        assert!(!valid.load(Ordering::Acquire));
        assert!(disk.load(&direct).is_none());
        assert!(disk.load(&nested).is_some());
        assert!(disk.load(&unrelated).is_some());
        assert_eq!(
            remove_from_owner(cache, &folder, Scope::Descendants).recv_timeout(timeout),
            Ok(true)
        );
        assert!(disk.load(&nested).is_none());
        assert_eq!(
            remove_from_owner(tmp.0.join("unreachable"), &folder, Scope::File)
                .recv_timeout(timeout),
            Ok(false)
        );
    }

    #[test]
    fn 失効前の保存は破損した原本レコードを再公開しない() {
        let tmp = Temp::new();
        let disk = Disk::open(tmp.0.clone()).unwrap();
        let key = key(tmp.0.join("original"));
        let valid = Arc::new(AtomicBool::new(true));
        disk.track(&key, &valid);
        assert_eq!(
            disk.remove(key.path.clone(), Scope::File)
                .recv_timeout(Duration::from_secs(5)),
            Ok(true)
        );
        disk.save(key.clone(), Arc::new(fixture()), valid);
        let barrier = disk.remove(tmp.0.join("different"), Scope::File);
        assert_eq!(barrier.recv_timeout(Duration::from_secs(5)), Ok(true));
        assert!(disk.load(&key).is_none());
    }

    #[test]
    fn 保存先を開けなくても永続キャッシュなしで起動できる() {
        let tmp = Temp::new();
        let unavailable = tmp.0.join("file");
        fs::write(&unavailable, b"file").unwrap();
        assert!(Disk::open(unavailable).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = tmp.0.join("read-only");
            fs::create_dir(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
            let denied = fs::File::create(locked.join("probe")).is_err();
            if denied {
                assert!(Disk::open(locked.clone()).is_none());
            }
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn 終了時にcontrol_jsonを消し失敗後の再試行成功でremovingを解除する() {
        let tmp = Temp::new();
        let cache = tmp.0.join("cache");
        let disk = Disk::open(cache.clone()).unwrap();
        let control = cache.join("control.json");
        assert!(control.exists());
        let target = key(tmp.0.join("folder/a.png"));
        let sibling = key(tmp.0.join("folder/b.png"));
        let valid = Arc::new(AtomicBool::new(true));
        disk.save(target.clone(), Arc::new(fixture()), valid.clone());
        disk.save(sibling.clone(), Arc::new(fixture()), valid);
        let timeout = Duration::from_secs(5);
        assert_eq!(
            disk.remove(tmp.0.join("barrier"), Scope::File)
                .recv_timeout(timeout),
            Ok(true)
        );
        assert!(disk.load(&target).is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // キャッシュフォルダを一時的に書込禁止にして 1 回目の remove を失敗させる。
            fs::set_permissions(&cache, fs::Permissions::from_mode(0o500)).unwrap();
            let denied = fs::File::create(cache.join("probe")).is_err();
            if denied {
                assert_eq!(
                    disk.remove(target.path.clone(), Scope::File)
                        .recv_timeout(timeout),
                    Ok(false)
                );
                assert!(!disk.removing.lock().unwrap().is_empty());
                assert!(disk.load(&target).is_none());
                // 権限を戻して再試行すると removing の旧エントリもまとめて解除される。
                fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
                assert_eq!(
                    disk.remove(target.path.clone(), Scope::File)
                        .recv_timeout(timeout),
                    Ok(true)
                );
                assert!(disk.removing.lock().unwrap().is_empty());
                disk.save(
                    target.clone(),
                    Arc::new(fixture()),
                    Arc::new(AtomicBool::new(true)),
                );
                assert_eq!(
                    disk.remove(tmp.0.join("barrier2"), Scope::File)
                        .recv_timeout(timeout),
                    Ok(true)
                );
                assert!(disk.load(&target).is_some());

                // 単一ファイルが開けなくても残りの一致レコードを削除し切る。
                let mut store = store(cache.clone());
                let unreadable = record_path(&cache, &tmp.0.join("other/unreadable.png"));
                fs::write(&unreadable, b"LFP1").unwrap();
                fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
                let open_denied = File::open(&unreadable).is_err();
                let res = store.remove(&tmp.0.join("folder"), Scope::Children);
                fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
                let _ = fs::remove_file(&unreadable);
                if open_denied {
                    assert!(res.is_err());
                }
                assert!(!record_path(&cache, &target.path).exists());
                assert!(!record_path(&cache, &sibling.path).exists());
            } else {
                fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        drop(disk);
        assert!(!control.exists(), "Disk 破棄時に control.json を消す");
    }
}
