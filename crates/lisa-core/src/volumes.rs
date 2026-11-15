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
        // 「/」は読み取り専用のシステム側なので、Data を足して、こちらを読むものにする。
        let data = Path::new(DATA_VOLUME);
        if data.is_dir() && !volumes.iter().any(|v| v.mount_path == data) {
            let url = NSURL::fileURLWithPath(&NSString::from_str(DATA_VOLUME));
            if let Some(mut volume) = read(&url, &keys, &key_array) {
                volume.mount_path = data.to_path_buf();
                volume.hidden = false;
                volumes.insert(1.min(volumes.len()), volume);
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

    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;

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

    pub fn list() -> io::Result<Vec<Volume>> {
        let mut buffer = vec![0u16; 1024];
        // SAFETY: 長さを渡したバッファに書いてもらう。
        let len = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut volumes = Vec::new();
        for root in buffer[..len as usize]
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
        {
            let mut root_z = root.to_vec();
            root_z.push(0);
            if let Some(volume) = volume(&root_z) {
                volumes.push(volume);
            }
        }
        Ok(volumes)
    }

    /// `root` は「C:\」のような、0 で終わる UTF-16。
    fn volume(root: &[u16]) -> Option<Volume> {
        let mount_path = PathBuf::from(OsString::from_wide(&root[..root.len() - 1]));
        // SAFETY: 0 で終わる文字列を渡す。
        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
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
