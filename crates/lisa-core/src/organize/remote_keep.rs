//! 端末間の整理で残す現物の参照と保護。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{db::Db, dups::ContentKey, listing::Kind, query};

pub mod relay;

pub const MAX_PROTECTED_ENTRIES: usize = 1000;
pub const KEEP_LEASE_MS: i64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepReference {
    pub device_id: String,
    pub drive_id: String,
    pub chain: Vec<i64>,
    pub format: u32,
    pub key: ContentKey,
    /// 実体識別子。ボリューム番号とファイル番号。
    #[serde(default)]
    pub identity: [u64; 2],
}

#[derive(Serialize, Deserialize)]
pub struct RemotePlan {
    pub route: relay::Route,
    pub keep: KeepReference,
    pub sources: Vec<KeepReference>,
    /// 依頼元が重複パネルで見せた「残す」側の場所（端末 › ボリューム › … › 名前）。
    #[serde(default)]
    pub keep_label: Option<String>,
}

/// 承認画面と CLI に出す、残す側の要約。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeepSummary {
    /// 依頼元の申告による場所。
    pub label: Option<String>,
    /// 期待する中身のハッシュ（先頭 12 桁）。
    pub hash: String,
}

impl KeepSummary {
    pub fn line(&self) -> String {
        let place = self.label.as_deref().unwrap_or("（場所の申告なし）");
        format!("残す: {place}　ハッシュ {}…", self.hash)
    }
}

impl KeepSummary {
    /// 承認画面に出す中立の注記。
    pub fn note(&self) -> String {
        let place = self.label.as_deref().unwrap_or("依頼元の申告なしの場所");
        format!("残す側は {place} で、実行直前に同じ中身か確認します")
    }
}

/// 遠隔の残す側がある計画だけ要約を返す。
pub fn keep_summary(db: &Db, plan_id: i64) -> Option<KeepSummary> {
    let plan = remote_plan(db, plan_id).ok()??;
    Some(KeepSummary {
        label: plan.keep_label,
        hash: blake3::Hash::from_bytes(plan.keep.key.hash).to_hex()[..12].to_string(),
    })
}

pub(super) fn remote_plan(db: &Db, plan_id: i64) -> Result<Option<RemotePlan>, String> {
    use rusqlite::OptionalExtension;
    let intent: Option<String> = db
        .conn()
        .query_row(
            "SELECT intent FROM remote_keep_plan WHERE plan_id = ?1",
            [plan_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(display)?;
    intent
        .map(|data| serde_json::from_str(&data).map_err(display))
        .transpose()
}

pub(super) struct ActiveLease<'a> {
    ctx: &'a super::Context,
    plan: &'a RemotePlan,
}

impl<'a> ActiveLease<'a> {
    pub(super) fn acquire(ctx: &'a super::Context, plan: &'a RemotePlan) -> Result<Self, String> {
        call(ctx, plan, relay::RelayAction::Reserve)?;
        Ok(Self { ctx, plan })
    }
}

impl Drop for ActiveLease<'_> {
    fn drop(&mut self) {
        let _ = call(self.ctx, self.plan, relay::RelayAction::Release);
    }
}

/// 逆方向の端末間計画とは別に、同じ ID の承認が並行して中継を操作するのを防ぐ。
pub(super) fn lock_attempt(data_dir: &Path, id: i64) -> Result<std::fs::File, String> {
    let path = data_dir.join("remote-organize.lock");
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(display)?;
    file.try_lock()
        .map_err(|error| format!("計画 #{id} の実行権を取れません: {error}"))?;
    Ok(file)
}

pub(super) fn call(
    ctx: &super::Context,
    plan: &RemotePlan,
    action: relay::RelayAction,
) -> Result<(), String> {
    let client = ctx
        .keep_client
        .ok_or("依頼元への接続を使えません。Mac の Lisa を起動して再度承認してください")?;
    let started = std::time::Instant::now();
    client(&ctx.data_dir, &plan.route, &plan.keep, action).map_err(|error| format!(
        "依頼元または残す側を確認できません。Mac の Lisa を起動して重複を再取得してください: {error}"
    ))?;
    if matches!(
        action,
        relay::RelayAction::Reserve | relay::RelayAction::Renew
    ) && started.elapsed().as_millis() >= (KEEP_LEASE_MS - 1000) as u128
    {
        return Err("中継の確認中に予約期限が近づきました。重複を再取得してください".into());
    }
    Ok(())
}

pub fn resolve(db: &Db, keep: &KeepReference) -> Result<PathBuf, String> {
    if keep.format != crate::dups::COMPARISON_FORMAT || keep.key.size == 0 {
        return Err("残す項目のハッシュ方式またはサイズに対応していません".into());
    }
    if db.meta("this_device").map_err(display)? != Some(keep.device_id.clone()) {
        return Err("残す項目の端末 ID が一致しません".into());
    }
    let entry_id = keep
        .chain
        .last()
        .copied()
        .filter(|_| keep.chain.len() > 1)
        .ok_or("ボリュームのルートは残す項目に指定できません")?;
    let location = query::path_of(db, entry_id)
        .map_err(display)?
        .ok_or("残す項目を DB から確認できません")?;
    let entry = db
        .entry(entry_id)
        .map_err(display)?
        .ok_or("残す項目が変更されました")?;
    let kind = if keep.key.directory {
        Kind::Dir
    } else {
        Kind::File
    };
    if location.drive_id != keep.drive_id
        || location.chain != keep.chain
        || entry.kind != kind
        || entry.hash != Some(keep.key.hash)
        || entry.cloud_only
        || entry.other_volume
        || entry.error.is_some()
        || (kind == Kind::File && entry.size != keep.key.size)
    {
        return Err("残す項目が変更されました。重複を再取得して確認してください".into());
    }
    Ok(location.path)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(windows)]
mod windows;

#[cfg(all(feature = "verify-remote-keep", not(debug_assertions)))]
compile_error!("verify-remote-keep は検証用ビルドだけで使えます");
#[cfg(all(unix, feature = "verify-remote-keep"))]
mod fixture;

#[cfg(windows)]
use windows::Protected;
#[cfg(not(windows))]
struct Protected;

impl Protected {
    #[cfg(all(unix, feature = "verify-remote-keep"))]
    fn open(_db: &Db, keep: &KeepReference, path: &Path) -> Result<Self, String> {
        fixture::verify(keep, path)?;
        Ok(Self)
    }

    #[cfg(all(unix, feature = "verify-remote-keep"))]
    fn verify(&mut self, keep: &KeepReference, path: &Path) -> Result<(), String> {
        fixture::verify(keep, path)
    }

    // 合成アダプターには共有拒否の保護がないので、更新のたびに全体を検証する。
    #[cfg(all(unix, feature = "verify-remote-keep"))]
    fn verify_held(&mut self, keep: &KeepReference, path: &Path) -> Result<(), String> {
        fixture::verify(keep, path)
    }

    #[cfg(all(not(windows), not(all(unix, feature = "verify-remote-keep"))))]
    fn open(_db: &Db, _keep: &KeepReference, _path: &Path) -> Result<Self, String> {
        let os = if cfg!(target_os = "macos") {
            "macOS"
        } else {
            std::env::consts::OS
        };
        Err(format!(
            "残す側が {os} のため、整理は依頼できません。比較表示のみ"
        ))
    }

    #[cfg(all(not(windows), not(all(unix, feature = "verify-remote-keep"))))]
    fn verify(&mut self, _keep: &KeepReference, _path: &Path) -> Result<(), String> {
        Err("残す側を安定して読み取れません。比較表示のみ".into())
    }

    #[cfg(all(not(windows), not(all(unix, feature = "verify-remote-keep"))))]
    fn verify_held(&mut self, _keep: &KeepReference, _path: &Path) -> Result<(), String> {
        Err("残す側を安定して読み取れません。比較表示のみ".into())
    }
}

pub struct ProtectedKeep {
    protected: Protected,
    path: PathBuf,
}

impl ProtectedKeep {
    pub fn inspect(db: &Db, keep: KeepReference) -> Result<KeepReference, String> {
        #[cfg(windows)]
        let keep = {
            let mut keep = keep;
            let path = resolve(db, &keep)?;
            let mount = windows::local_mount(db, &keep)?;
            keep.identity = windows::identify(&mount, &path)?;
            keep
        };
        #[cfg(all(unix, feature = "verify-remote-keep"))]
        let keep = {
            let mut keep = keep;
            keep.identity = fixture::identify(&resolve(db, &keep)?)?;
            keep
        };
        Self::open(db, &keep)?;
        Ok(keep)
    }

    pub fn open(db: &Db, keep: &KeepReference) -> Result<Self, String> {
        #[cfg(not(any(windows, all(unix, feature = "verify-remote-keep"))))]
        let path = Path::new("");
        #[cfg(any(windows, all(unix, feature = "verify-remote-keep")))]
        let resolved = resolve(db, keep)?;
        #[cfg(any(windows, all(unix, feature = "verify-remote-keep")))]
        let path = resolved.as_path();
        let protected = Protected::open(db, keep, path)?;
        Ok(Self {
            protected,
            path: path.to_path_buf(),
        })
    }

    fn assert_path(&self, path: &Path) -> Result<(), String> {
        if self.path != path {
            return Err("残す項目の場所が変更されました。再取得して確認してください".into());
        }
        Ok(())
    }

    pub fn verify(&mut self, db: &Db, keep: &KeepReference) -> Result<(), String> {
        let path = resolve(db, keep)?;
        self.assert_path(&path)?;
        self.protected.verify(keep, &path)
    }

    /// 予約の更新用。保護が保持されている環境だけ再ハッシュを省く（取得時に完全検証済み）。
    pub fn verify_held(&mut self, db: &Db, keep: &KeepReference) -> Result<(), String> {
        let path = resolve(db, keep)?;
        self.assert_path(&path)?;
        self.protected.verify_held(keep, &path)
    }
}

pub(super) fn isolate_source(
    db: &Db,
    ctx: &super::Context,
    op: &super::PlanOp,
    item: &KeepReference,
    pre_rename: impl FnOnce() -> Result<(), String>,
) -> Result<PathBuf, String> {
    let path = resolve(db, item)?;
    if path != op.src {
        return Err("隔離する項目の場所が変更されました".into());
    }
    #[cfg(windows)]
    {
        let protected = Protected::source(db, item, &path)?;
        let mount = windows::local_mount(db, item)?;
        return super::quarantine::move_to_quarantine(
            op.plan_id,
            op.seq,
            op.op,
            &path,
            Some(&mount),
            &ctx.data_dir,
            move |_, destination| {
                pre_rename().map_err(std::io::Error::other)?;
                protected.isolate(destination)
            },
        );
    }
    #[cfg(all(unix, feature = "verify-remote-keep"))]
    {
        fixture::verify(item, &path)?;
        let mount = crate::query::drives(db)
            .map_err(display)?
            .into_iter()
            .find(|drive| drive.id == item.drive_id)
            .and_then(|drive| drive.mount)
            .ok_or("合成ボリュームが見つかりません")?;
        return super::quarantine::move_to_quarantine(
            op.plan_id,
            op.seq,
            op.op,
            &path,
            Some(&mount),
            &ctx.data_dir,
            |src, dst| {
                pre_rename().map_err(std::io::Error::other)?;
                super::apply::rename_without_replace(src, dst)
            },
        );
    }
    #[cfg(not(any(windows, all(unix, feature = "verify-remote-keep"))))]
    {
        let _ = (ctx, pre_rename);
        Err("隔離側の現物の同一性を実行中に保証できません。比較表示のみ".into())
    }
}

/// 物理ハンドルを開いたまま、別プロセスからも分かる予約を記録する。
pub fn register(
    db: &Db,
    data_dir: &Path,
    keep: &KeepReference,
    token: &str,
    protected: &ProtectedKeep,
) -> Result<i64, String> {
    let path = resolve(db, keep)?;
    protected.assert_path(&path)?;
    let _lock = super::apply::try_lock_plans(data_dir)?;
    let expires_at = crate::db::now_ms() + KEEP_LEASE_MS;
    let tx = db.conn().unchecked_transaction().map_err(display)?;
    tx.execute(
        "DELETE FROM keep_reservation WHERE expires_at <= ?1",
        [crate::db::now_ms()],
    )
    .map_err(display)?;
    check_overlap(&tx, [path.as_path()])?;
    tx.execute(
        "INSERT INTO keep_reservation(token,path,expires_at) VALUES (?1,?2,?3)",
        rusqlite::params![
            token,
            path.to_str().ok_or("残す項目のパスを保存できません")?,
            expires_at
        ],
    )
    .map_err(display)?;
    tx.commit().map_err(display)?;
    Ok(expires_at)
}

pub fn renew(db: &Db, token: &str) -> Result<i64, String> {
    let now = crate::db::now_ms();
    let expires_at = now + KEEP_LEASE_MS;
    let changed = db
        .conn()
        .execute(
            "UPDATE keep_reservation SET expires_at = ?1 WHERE token = ?2 AND expires_at > ?3",
            rusqlite::params![expires_at, token, now],
        )
        .map_err(display)?;
    if changed == 0 {
        return Err("留保が失効しました。重複を再取得して確認してください".into());
    }
    Ok(expires_at)
}

pub fn release(db: &Db, token: &str) -> Result<(), String> {
    db.conn()
        .execute("DELETE FROM keep_reservation WHERE token = ?1", [token])
        .map_err(display)?;
    Ok(())
}

/// 期限切れの片付けでは、消えたデータディレクトリを作り直さない。
pub fn release_on_disk(path: &Path, token: &str) -> Result<(), String> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(display)?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(display)?;
    conn.execute("DELETE FROM keep_reservation WHERE token = ?1", [token])
        .map_err(display)?;
    Ok(())
}

pub(super) fn check_overlap<'a>(
    conn: &rusqlite::Connection,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare_cached("SELECT path FROM keep_reservation WHERE expires_at > ?1")
        .map_err(display)?;
    let paths: Vec<_> = paths.into_iter().map(comparable_path).collect();
    let rows = stmt
        .query_map([crate::db::now_ms()], |row| row.get::<_, String>(0))
        .map_err(display)?;
    for reserved in rows {
        let reserved = comparable_path(Path::new(&reserved.map_err(display)?));
        if paths
            .iter()
            .any(|path| path.starts_with(&reserved) || reserved.starts_with(path))
        {
            return Err("残す項目またはその親子が他の計画で予約されています。重複を再取得して確認してください".into());
        }
    }
    Ok(())
}

fn comparable_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    return PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase());
    #[cfg(not(windows))]
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        listing::read_dir,
        organize::{Context, OpInput, approve_plan, create_plan, execute_plan},
        volumes::Volume,
    };

    #[test]
    fn parent_and_child_reservations_block_all_shared_plan_entry_points_until_expiry() {
        let dir = std::env::temp_dir().join(format!("lisa-keep-{}", uuid::Uuid::new_v4()));
        let mount = dir.join("fixture");
        let ancestor = mount.join("ancestor");
        let reserved = ancestor.join("contents");
        std::fs::create_dir_all(&reserved).unwrap();
        let child = reserved.join("one.txt");
        std::fs::write(&child, b"sample").unwrap();
        let mut db = Db::open_in_memory().unwrap();
        let device = db.this_device("旧PC", "旧PC", "fixture").unwrap();
        db.upsert_drive(
            &device,
            &Volume {
                id: Some("fixture".into()),
                label: "fixture".into(),
                fs: "fixture".into(),
                mount_path: mount.clone(),
                capacity: None,
                available: None,
                removable: false,
                network: false,
                read_only: false,
                case_sensitive: None,
                hidden: false,
            },
        )
        .unwrap();
        for path in [&mount, &ancestor, &reserved] {
            db.save_listing(
                "fixture",
                &mount,
                path,
                &read_dir(path).map_err(|e| e.to_string()),
            )
            .unwrap();
        }
        let ctx = Context::with_builtin(dir.join("data"));
        let ops = [OpInput::Move {
            from: child.clone(),
            to: mount.join("moved.txt"),
        }];
        let approved = create_plan(&mut db, &ctx, "fixture", None, &ops, false)
            .unwrap()
            .id
            .unwrap();
        approve_plan(&mut db, approved, false).unwrap();
        let pending = create_plan(&mut db, &ctx, "fixture", None, &ops, false)
            .unwrap()
            .id
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO keep_reservation VALUES (?1,?2,?3)",
                rusqlite::params![
                    "test-reservation",
                    reserved.to_str().unwrap(),
                    crate::db::now_ms() + KEEP_LEASE_MS
                ],
            )
            .unwrap();
        assert!(check_overlap(db.conn(), [ancestor.as_path()]).is_err());
        assert!(check_overlap(db.conn(), [child.as_path()]).is_err());
        assert!(check_overlap(db.conn(), [mount.join("elsewhere").as_path()]).is_ok());
        assert!(create_plan(&mut db, &ctx, "fixture", None, &ops, false).is_err());
        assert!(approve_plan(&mut db, pending, false).is_err());
        assert!(execute_plan(&mut db, &ctx, approved).is_err());
        assert!(child.is_file());
        db.conn()
            .execute("UPDATE keep_reservation SET expires_at = 0", [])
            .unwrap();
        assert!(renew(&db, "test-reservation").is_err());
        assert!(check_overlap(db.conn(), [child.as_path()]).is_ok());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
