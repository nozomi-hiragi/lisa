//! この PC につながっているドライブ（ボリューム）の一覧。
//!
//! ID は OS がくれる変わらない値を使う。Mac はボリューム UUID、Windows は NTFS なら 64 ビットの
//! ボリュームシリアル、それ以外は 32 ビットのシリアル。同じ外付けを Mac と Windows で同じものと
//! 見分けられるかは、まだ実機で確かめていない（`docs/scan.md`）。

use std::io;
use std::path::PathBuf;

/// 1 つのボリューム。
#[derive(Clone, Debug)]
pub struct Volume {
    /// OS がくれる ID。Mac は `mac:<UUID>`、Windows は `ntfs:<16 桁>` か `serial:<8 桁>`。
    /// 取れなければ None（ネットワークドライブなど）。
    pub id: Option<String>,
    /// ボリュームの名前（表示用）。
    pub label: String,
    /// ファイルシステム（`apfs`、`exfat`、`NTFS` など。OS が返す綴りのまま）。
    pub fs: String,
    /// 今つながっている場所（`/Volumes/…`、`D:\` など）。
    pub mount_path: PathBuf,
    /// 容量と空き（バイト）。
    pub capacity: Option<u64>,
    pub available: Option<u64>,
    /// 取り外せる（外付け）。Windows は USB の SSD を「固定」と返すことがある。
    pub removable: bool,
    /// ネットワークの向こうにある。
    pub network: bool,
    /// 読み取り専用。
    pub read_only: bool,
    /// 大文字小文字を区別するか。分からなければ None。
    pub case_sensitive: Option<bool>,
    /// 既定では読まない（Mac のシステムのボリューム、Preboot など Finder に出ないもの）。
    pub hidden: bool,
}

/// この端末の名前（最初の表示名に使う）。取れなければ「この端末」。
pub fn device_name() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        unsafe extern "C" {
            fn gethostname(name: *mut std::ffi::c_char, len: usize) -> std::ffi::c_int;
        }
        // SAFETY: 長さを渡したバッファに書いてもらう。
        if unsafe { gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            let name = String::from_utf8_lossy(&buffer[..end]);
            // Mac は「simura.local」のように付くので、最初の部分だけ。
            let name = name.split('.').next().unwrap_or_default();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    #[cfg(windows)]
    if let Ok(name) = std::env::var("COMPUTERNAME")
        && !name.is_empty()
    {
        return name;
    }
    "この端末".to_string()
}

/// LAN で使っているこの PC のアドレス。外へ向かう UDP の口を作るだけで、何も送らない。
pub fn lan_address() -> Option<std::net::IpAddr> {
    use std::net::{Ipv4Addr, UdpSocket};
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// つながっているボリュームの一覧。
pub fn list() -> io::Result<Vec<Volume>> {
    imp::list()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{
        NSArray, NSDictionary, NSFileManager, NSNumber, NSString, NSURL, NSURLResourceKey,
        NSVolumeEnumerationOptions,
    };
    use std::path::Path;

    pub fn list() -> io::Result<Vec<Volume>> {
        // SAFETY: Foundation が持っている定数の文字列を読むだけ。
        let keys = unsafe {
            use objc2_foundation::*;
            [
                NSURLVolumeUUIDStringKey,
                NSURLVolumeLocalizedNameKey,
                NSURLVolumeTypeNameKey,
                NSURLVolumeTotalCapacityKey,
                NSURLVolumeAvailableCapacityKey,
                NSURLVolumeIsRemovableKey,
                NSURLVolumeIsEjectableKey,
                NSURLVolumeIsInternalKey,
                NSURLVolumeIsLocalKey,
                NSURLVolumeIsReadOnlyKey,
                NSURLVolumeSupportsCaseSensitiveNamesKey,
                NSURLVolumeIsBrowsableKey,
                NSURLVolumeIsRootFileSystemKey,
            ]
        };
        let key_array = NSArray::from_slice(&keys);
        let manager = NSFileManager::defaultManager();
        let urls = manager
            .mountedVolumeURLsIncludingResourceValuesForKeys_options(
                Some(&key_array),
                NSVolumeEnumerationOptions::empty(),
            )
            .ok_or_else(|| io::Error::other("ボリュームの一覧が取れませんでした"))?;
        let mut volumes: Vec<Volume> = urls
            .iter()
            .filter_map(|url| read(&url, &keys, &key_array))
            .collect();
        // 起動ディスクの Data のボリューム（ユーザーのファイルがあるところ）は、一覧に出てこない。
        // 代わりに出てくる「/」は読み取り専用のシステム側で、しかも ID としてボリュームグループの
        // UUID（= Data の UUID）を返すので、Data とぶつかる。「/」を Data に置き換える。
        // 名前は Finder と同じ起動ディスクの名前（「Macintosh HD」など）になる。
        let data = Path::new(DATA_VOLUME);
        if data.is_dir() && !volumes.iter().any(|v| v.mount_path == data) {
            let url = NSURL::fileURLWithPath(&NSString::from_str(DATA_VOLUME));
            if let Some(mut volume) = read(&url, &keys, &key_array) {
                volume.mount_path = data.to_path_buf();
                volume.hidden = false;
                match volumes.iter().position(|v| v.mount_path == Path::new("/")) {
                    Some(root) => volumes[root] = volume,
                    None => volumes.insert(0, volume),
                }
            }
        }
        Ok(volumes)
    }

    /// 起動ディスクの Data のボリュームのマウント位置。
    const DATA_VOLUME: &str = "/System/Volumes/Data";

    type Keys = [&'static NSURLResourceKey; 13];

    fn read(url: &NSURL, keys: &Keys, key_array: &NSArray<NSURLResourceKey>) -> Option<Volume> {
        let path = url.path()?;
        let values = url.resourceValuesForKeys_error(key_array).ok()?;
        let [
            uuid,
            name,
            type_name,
            total,
            available,
            removable,
            ejectable,
            internal,
            local,
            read_only,
            case_sensitive,
            browsable,
            root,
        ] = *keys;
        let is_root = flag(&values, root).unwrap_or(false);
        let removable = flag(&values, removable).unwrap_or(false)
            || flag(&values, ejectable).unwrap_or(false)
            || flag(&values, internal) == Some(false);
        Some(Volume {
            id: string(&values, uuid).map(|u| format!("mac:{u}")),
            label: string(&values, name).unwrap_or_default(),
            fs: string(&values, type_name).unwrap_or_default(),
            mount_path: PathBuf::from(path.to_string()),
            capacity: number(&values, total),
            available: number(&values, available),
            removable: removable && local_or(&values, local),
            network: !local_or(&values, local),
            read_only: flag(&values, read_only).unwrap_or(false),
            case_sensitive: flag(&values, case_sensitive),
            // 起動ディスクの「/」は読み取り専用のシステムのボリューム。中身は Data のほうを読む。
            hidden: is_root || flag(&values, browsable) == Some(false),
        })
    }

    fn value(
        values: &NSDictionary<NSURLResourceKey, AnyObject>,
        key: &NSURLResourceKey,
    ) -> Option<Retained<AnyObject>> {
        values.objectForKey(key)
    }

    fn string(
        values: &NSDictionary<NSURLResourceKey, AnyObject>,
        key: &NSString,
    ) -> Option<String> {
        value(values, key)?
            .downcast::<NSString>()
            .ok()
            .map(|s| s.to_string())
    }

    fn number(values: &NSDictionary<NSURLResourceKey, AnyObject>, key: &NSString) -> Option<u64> {
        value(values, key)?
            .downcast::<NSNumber>()
            .ok()
            .map(|n| n.unsignedLongLongValue())
    }

    fn flag(values: &NSDictionary<NSURLResourceKey, AnyObject>, key: &NSString) -> Option<bool> {
        value(values, key)?
            .downcast::<NSNumber>()
            .ok()
            .map(|n| n.boolValue())
    }

    fn local_or(values: &NSDictionary<NSURLResourceKey, AnyObject>, key: &NSString) -> bool {
        flag(values, key).unwrap_or(true)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileIdInfo, GetDiskFreeSpaceExW, GetDriveTypeW,
        GetFileInformationByHandleEx, GetLogicalDriveStringsW, GetVolumeInformationW,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_REMOTE, DRIVE_REMOVABLE};

    // GetVolumeInformationW のフラグ（winnt.h）。
    const FILE_CASE_SENSITIVE_SEARCH: u32 = 0x0000_0001;
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

    /// ネットワークドライブを調べるのを待つ長さ。つながっていないドライブは、ここで諦める。
    const REMOTE_TIMEOUT: Duration = Duration::from_secs(2);

    /// 今まだ `GetVolumeInformationW` の返事を待っているネットワークドライブ（0 で終わる UTF-16）。
    /// つながっていないドライブの問い合わせスレッドがまだ残っている間は、同じドライブの新しいスレッドを
    /// 重ねて作らないようにする。
    static REMOTE_IN_FLIGHT: LazyLock<Mutex<HashSet<Vec<u16>>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    pub fn list() -> io::Result<Vec<Volume>> {
        let mut buffer = vec![0u16; 1024];
        // SAFETY: 長さを渡したバッファに書いてもらう。
        let len = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let roots: Vec<Vec<u16>> = buffer[..len as usize]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(|root| {
                let mut root_z = root.to_vec();
                root_z.push(0);
                root_z
            })
            .collect();

        // ネットワークドライブが何本あっても合計の待ち時間が `REMOTE_TIMEOUT` で済むよう、先に並べて投げる。
        let mut slots: Vec<Option<Volume>> = vec![None; roots.len()];
        let mut pending_remote: Vec<(usize, std::sync::mpsc::Receiver<Option<Volume>>)> =
            Vec::new();
        for (idx, root_z) in roots.into_iter().enumerate() {
            // SAFETY: 0 で終わる文字列を渡す。GetDriveTypeW はディスクを読まないので待たない。
            let drive_type = unsafe { GetDriveTypeW(root_z.as_ptr()) };
            if drive_type != DRIVE_REMOTE {
                slots[idx] = query(&root_z, drive_type);
            } else if let Some(rx) = spawn_remote_query(root_z, drive_type) {
                pending_remote.push((idx, rx));
            }
        }
        if !pending_remote.is_empty() {
            let deadline = Instant::now() + REMOTE_TIMEOUT;
            for (idx, rx) in pending_remote {
                let remain = deadline.saturating_duration_since(Instant::now());
                if let Ok(Some(vol)) = rx.recv_timeout(remain) {
                    slots[idx] = Some(vol);
                }
            }
        }
        Ok(slots.into_iter().flatten().collect())
    }

    /// ネットワークドライブは、つながっていないと `GetVolumeInformationW` が長く返らない
    /// （管理者で動かすと割り当てたドライブが見えないので、普通に起動したときだけ止まっていた）。
    /// 別のスレッドで聞き、待ちきれなければ飛ばす。同じドライブの問い合わせがまだ残っていればもう出さない。
    fn spawn_remote_query(
        root: Vec<u16>,
        drive_type: u32,
    ) -> Option<std::sync::mpsc::Receiver<Option<Volume>>> {
        {
            let mut in_flight = REMOTE_IN_FLIGHT.lock().ok()?;
            if !in_flight.insert(root.clone()) {
                return None;
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("lisa-volume".into())
            .spawn(move || {
                struct InFlightGuard(Vec<u16>);
                impl Drop for InFlightGuard {
                    fn drop(&mut self) {
                        if let Ok(mut set) = REMOTE_IN_FLIGHT.lock() {
                            set.remove(&self.0);
                        }
                    }
                }
                let guard = InFlightGuard(root);
                let _ = tx.send(query(&guard.0, drive_type));
            })
            .is_err()
        {
            return None;
        }
        Some(rx)
    }

    fn query(root: &[u16], drive_type: u32) -> Option<Volume> {
        let mount_path = PathBuf::from(OsString::from_wide(&root[..root.len() - 1]));
        let mut name = [0u16; 261];
        let mut fs = [0u16; 261];
        let mut serial = 0u32;
        let mut flags = 0u32;
        // SAFETY: 長さを渡したバッファに書いてもらう。
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                &mut serial,
                ptr::null_mut(),
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            // ディスクの入っていないカードリーダーなど。
            return None;
        }
        let fs = from_wide(&fs);
        let id = if fs.eq_ignore_ascii_case("NTFS") || fs.eq_ignore_ascii_case("ReFS") {
            volume_serial_64(root).map(|s| format!("ntfs:{s:016x}"))
        } else {
            None
        }
        .or(Some(format!("serial:{serial:08x}")));
        let (mut available, mut total) = (0u64, 0u64);
        // SAFETY: 0 で終わる文字列と、書いてもらう u64 を渡す。
        let space = unsafe {
            GetDiskFreeSpaceExW(root.as_ptr(), &mut available, &mut total, ptr::null_mut())
        } != 0;
        Some(Volume {
            id,
            label: from_wide(&name),
            fs,
            mount_path,
            capacity: space.then_some(total),
            available: space.then_some(available),
            removable: drive_type == DRIVE_REMOVABLE,
            network: drive_type == DRIVE_REMOTE,
            read_only: flags & FILE_READ_ONLY_VOLUME != 0,
            case_sensitive: Some(flags & FILE_CASE_SENSITIVE_SEARCH != 0),
            hidden: false,
        })
    }

    /// NTFS の 64 ビットのボリュームシリアル。一番上のフォルダを開いて聞く。
    fn volume_serial_64(root: &[u16]) -> Option<u64> {
        // SAFETY: 0 で終わる文字列を渡し、開いたハンドルは必ず閉じる。
        unsafe {
            let handle = CreateFileW(
                root.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut info: FILE_ID_INFO = std::mem::zeroed();
            let ok = GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            );
            CloseHandle(handle);
            (ok != 0).then_some(info.VolumeSerialNumber)
        }
    }

    fn from_wide(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        OsString::from_wide(&buffer[..end])
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub fn list() -> io::Result<Vec<Volume>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ボリュームの一覧は Mac と Windows だけ",
        ))
    }
}
