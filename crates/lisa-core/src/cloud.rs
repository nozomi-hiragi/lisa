//! クラウドにだけあるファイルを、うっかりダウンロードさせないための設定。

use std::io;

/// このプロセスでは、クラウドにだけあるファイルやフォルダを読んでもダウンロードしないようにする。
/// 読もうとするとエラーになる。起動したらすぐ呼ぶ。
///
/// Mac: `setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_PROCESS,
/// IOPOL_MATERIALIZE_DATALESS_FILES_OFF)`。ほかの OS では何もしない（Windows は属性を見て開かない）。
pub fn prevent_downloads() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::c_int;
        // <sys/resource.h>
        const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
        const IOPOL_SCOPE_PROCESS: c_int = 0;
        const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;
        unsafe extern "C" {
            fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
        }
        // SAFETY: 引数は整数だけで、ポインタを渡さない。
        let result = unsafe {
            setiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_PROCESS,
                IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
