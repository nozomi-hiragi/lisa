//! 端末を切り替えたときの読み取りと、LAN の索引の突き合わせ。

use super::*;
use crate::lan::{LanState, SavedPeer, call_mcp_tool};
use lisa_core::dups::{self, CatalogPage, ContentKey, DeviceGroups, Locations};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// 端末ごとの索引の取得の進み具合（取ったページ数、全ページ数）。
type Progress = Arc<Mutex<[Option<(usize, usize)>; 2]>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scope {
    pub target: Option<String>,
    pub comparison: Option<Option<String>>,
}

#[derive(Clone)]
struct Device {
    peer: Option<SavedPeer>,
    name: String,
}

impl Device {
    fn requested(lan: Option<&LanState>, id: Option<&str>) -> Result<Self, String> {
        match id {
            Some(id) => {
                let peer = lan
                    .and_then(|lan| lan.saved_peer(id))
                    .ok_or("ペアリングが解除されました")?;
                Ok(Self {
                    name: peer.name.clone(),
                    peer: Some(peer),
                })
            }
            None => Ok(Self {
                name: "この端末".into(),
                peer: None,
            }),
        }
    }

    fn call<T: DeserializeOwned>(&self, name: &str, arguments: Value) -> Result<T, String> {
        let peer = self.peer.as_ref().ok_or("遠隔端末の指定がありません")?;
        let value = call_mcp_tool(&peer.addr, peer.port, &peer.key, name, arguments).map_err(
            |(_, error)| {
                if error.starts_with("知らない道具:") {
                    "重複一覧に未対応".into()
                } else if error == dups::GENERATION_CHANGED {
                    error
                } else {
                    format!("取得失敗（接続を確認してください）: {error}")
                }
            },
        )?;
        serde_json::from_value(value).map_err(|error| format!("重複一覧に未対応: {error}"))
    }

    fn first_page(&self) -> Result<CatalogPage, String> {
        let db = self.peer.is_none().then(open_db).transpose()?;
        self.page(db.as_ref(), None, 0)
    }

    fn catalog(
        &self,
        mut page: CatalogPage,
        cancelled: &AtomicBool,
        progress: impl Fn(usize, usize),
    ) -> Result<CatalogPage, String> {
        let db = self.peer.is_none().then(open_db).transpose()?;
        let mut checked = 0;
        let per_page = page.keys.len().max(1);
        let pages = page.total.div_ceil(per_page).max(1);
        let mut done = 1;
        progress(done, pages);
        while let Some(offset) = page.next {
            check_cancelled(cancelled)?;
            if offset <= checked || offset != page.keys.len() {
                return Err("索引のページ番号が正しくありません".into());
            }
            checked = offset;
            let next = self.page(db.as_ref(), Some(&page.generation), offset)?;
            if next.generation != page.generation {
                return Err(dups::GENERATION_CHANGED.into());
            }
            page.keys.extend(next.keys);
            page.counts.extend(next.counts);
            page.next = next.next;
            done += 1;
            progress(done, pages.max(done));
        }
        if page.keys.windows(2).any(|keys| keys[0] >= keys[1]) {
            return Err("索引のキーの順序が正しくありません".into());
        }
        Ok(page)
    }

    fn page(
        &self,
        db: Option<&lisa_core::db::Db>,
        generation: Option<&str>,
        offset: usize,
    ) -> Result<CatalogPage, String> {
        let page = match db {
            Some(db) => dups::catalog_page(db, generation, offset, "")?,
            None => self.call(
                "lisa_duplicate_catalog",
                json!({ "generation": generation, "offset": offset }),
            )?,
        };
        if page.format != dups::COMPARISON_FORMAT {
            return Err("ハッシュ方式に未対応".into());
        }
        Ok(page)
    }

    fn locations(
        &self,
        generation: &str,
        keys: &[ContentKey],
        cancelled: &AtomicBool,
    ) -> Result<Vec<Locations>, String> {
        let db = self.peer.is_none().then(open_db).transpose()?;
        let mut all = Vec::new();
        for keys in keys.chunks(dups::DETAIL_SIZE) {
            check_cancelled(cancelled)?;
            let entries = match &db {
                Some(db) => dups::locations(db, generation, keys)?,
                None => self.call(
                    "lisa_duplicate_locations",
                    json!({ "generation": generation, "keys": keys }),
                )?,
            };
            if entries.len() != keys.len()
                || entries
                    .iter()
                    .zip(keys)
                    .any(|(entry, key)| entry.key != *key)
            {
                return Err("索引と所在が一致しません".into());
            }
            all.extend(entries);
        }
        // 写しがまだあることだけ確かめる（キーは返らない位置を頼む）。
        self.page(db.as_ref(), Some(generation), u32::MAX as usize)?;
        Ok(all)
    }
}

/// 両端末にあるキーと、写しの所在の数から見積もった空く量の上限。上限の大きい順。
/// 所在の数を返さない相手なら上限なし（全部取る）として扱う。
fn candidates(left: &CatalogPage, right: &CatalogPage) -> Vec<(ContentKey, u64)> {
    let count = |page: &CatalogPage, index: usize| {
        (page.counts.len() == page.keys.len()).then(|| page.counts[index] as usize)
    };
    let mut found: Vec<_> = left
        .keys
        .iter()
        .enumerate()
        .filter_map(|(l, key)| {
            let r = right.keys.binary_search(key).ok()?;
            let bound = match (count(left, l), count(right, r)) {
                (Some(a), Some(b)) => dups::reclaim(key, a + b),
                _ => u64::MAX,
            };
            Some((key.clone(), bound))
        })
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found
}

fn clone_locations(list: &[Locations]) -> Vec<Locations> {
    list.iter()
        .map(|item| Locations {
            key: item.key.clone(),
            entries: item.entries.clone(),
        })
        .collect()
}

pub(super) fn open_db() -> Result<lisa_core::db::Db, String> {
    let path = crate::scan::db_path().ok_or("DB の場所が分かりません")?;
    lisa_core::db::Db::open(&path).map_err(|error| error.to_string())
}

#[derive(Clone)]
pub struct Copy {
    pub device_id: Option<String>,
    pub location: dups::Location,
}

pub struct RemoteGroup {
    pub key: ContentKey,
    pub copies: Vec<Copy>,
}

pub struct RemoteGroups {
    pub groups: Vec<RemoteGroup>,
    pub devices: Vec<(String, bool)>,
    pub fetched_at: std::time::SystemTime,
    generations: Option<[String; 2]>,
}

fn collect_copies(device: &Device, entries: Vec<dups::Location>) -> impl Iterator<Item = Copy> {
    let id = device.peer.as_ref().map(|peer| peer.device_id.clone());
    entries.into_iter().map(move |mut location| {
        if let Some(name) = location.parts.first_mut() {
            name.clone_from(&device.name);
        }
        Copy {
            device_id: id.clone(),
            location,
        }
    })
}

fn fetch(
    left: &Device,
    right: Option<&Device>,
    cancelled: &AtomicBool,
    cached: Option<&Arc<RemoteGroups>>,
    progress: &Progress,
) -> Response {
    let report = |side: usize| {
        move |done, pages| {
            if let Ok(mut progress) = progress.lock() {
                progress[side] = Some((done, pages));
            }
        }
    };
    let error_for = |device: &Device, error| (device.name.clone(), error);
    let mut devices = Vec::new();
    let mut generations = None;
    let groups = if let Some(right) = right {
        let left_page = left.first_page().map_err(|error| error_for(left, error))?;
        check_cancelled(cancelled).map_err(|error| error_for(right, error))?;
        let right_page = right
            .first_page()
            .map_err(|error| error_for(right, error))?;
        let current = [left_page.generation.clone(), right_page.generation.clone()];
        if left_page.complete && right_page.complete {
            if let Some(hit) = cached.filter(|c| c.generations.as_ref() == Some(&current)) {
                return Ok(Arc::clone(hit));
            }
            generations = Some(current);
        }
        // 2 端末の索引は並行して取る。
        let (left_page, right_page) = std::thread::scope(|scope| {
            let right_page = scope.spawn(|| right.catalog(right_page, cancelled, report(1)));
            let left_page = left.catalog(left_page, cancelled, report(0));
            (left_page, right_page.join())
        });
        let left_page = left_page.map_err(|error| error_for(left, error))?;
        let right_page = right_page
            .map_err(|_| error_for(right, "索引の取得が中断されました".into()))?
            .map_err(|error| error_for(right, error))?;
        devices.push((left.name.clone(), left_page.complete));
        devices.push((right.name.clone(), right_page.complete));
        let candidates = candidates(&left_page, &right_page);
        drop((left_page.keys, right_page.keys));
        // 空く量の上限が大きい順に所在を取り、上位の組が決まったら残りは取らない。
        let mut left_entries = Vec::new();
        let mut right_entries = Vec::new();
        let mut groups = Vec::new();
        for batch in candidates.chunks(dups::DETAIL_SIZE) {
            if groups.len() >= MAX_GROUPS
                && groups
                    .last()
                    .is_some_and(|(key, a, b): &(ContentKey, Vec<_>, Vec<_>)| {
                        dups::reclaim(key, a.len() + b.len()) > batch[0].1
                    })
            {
                break;
            }
            let keys: Vec<_> = batch.iter().map(|(key, _)| key.clone()).collect();
            left_entries.extend(
                left.locations(&left_page.generation, &keys, cancelled)
                    .map_err(|error| error_for(left, error))?,
            );
            right_entries.extend(
                right
                    .locations(&right_page.generation, &keys, cancelled)
                    .map_err(|error| error_for(right, error))?,
            );
            groups = dups::compare_locations(
                clone_locations(&left_entries),
                clone_locations(&right_entries),
                MAX_GROUPS,
            );
        }
        groups
            .into_iter()
            .map(|(key, a, b)| RemoteGroup {
                key,
                copies: collect_copies(left, a)
                    .chain(collect_copies(right, b))
                    .collect(),
            })
            .collect()
    } else {
        let page: DeviceGroups = left
            .call("lisa_duplicate_groups", json!({}))
            .map_err(|error| error_for(left, error))?;
        devices.push((left.name.clone(), page.complete));
        page.groups
            .into_iter()
            .map(|group| RemoteGroup {
                key: group.key,
                copies: collect_copies(left, group.entries).collect(),
            })
            .collect()
    };
    Ok(Arc::new(RemoteGroups {
        groups,
        devices,
        fetched_at: std::time::SystemTime::now(),
        generations,
    }))
}

#[derive(Default)]
pub(crate) enum ViewState {
    #[default]
    Local,
    /// 端末名と、索引の取得の進み具合（取ったページ数、全ページ数）。
    Computing(Vec<(String, Option<(usize, usize)>)>),
    Ready(Arc<RemoteGroups>),
    Failed(String),
}

type Request = (Scope, u64);
type Response = Result<Arc<RemoteGroups>, (String, String)>;

struct Active {
    request: Request,
    cancelled: Arc<AtomicBool>,
    receiver: Receiver<Response>,
    progress: Progress,
}

#[derive(Default)]
pub(crate) struct Loader {
    last: Option<Request>,
    revision: u64,
    active: Option<Active>,
    cached: Option<(Scope, Arc<RemoteGroups>)>,
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("重複一覧の取得を中止しました".into());
    }
    Ok(())
}

pub(crate) fn compute_remote(
    mut dups: ResMut<Dups>,
    lan: Option<Res<LanState>>,
    mut loader: Local<Loader>,
    occluded: Res<crate::view::WindowOccludedState>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if loader.revision != dups.revision {
        loader.revision = dups.revision;
        loader.cached = None;
    }
    let request =
        (dups.open && dups.scope != Scope::default()).then(|| (dups.scope.clone(), dups.refresh));
    if let Some(active) = &loader.active {
        let current =
            request.as_ref() == Some(&active.request) && !active.cancelled.load(Ordering::Relaxed);
        if !current {
            active.cancelled.store(true, Ordering::Relaxed);
        }
        use std::sync::mpsc::TryRecvError;
        match active.receiver.try_recv() {
            Err(TryRecvError::Empty) => {
                if request.is_some() && !occluded.0 {
                    redraw.write(RequestRedraw);
                }
                if request.is_some() && !current && !matches!(dups.remote, ViewState::Computing(_))
                {
                    dups.remote = computing(&dups.scope, lan.as_deref(), None);
                } else if current {
                    // 進み具合が変わったときだけ書き換える（パネルの作り直しを増やさない）。
                    let next = computing(&dups.scope, lan.as_deref(), Some(&active.progress));
                    if !matches!((&dups.remote, &next), (ViewState::Computing(a), ViewState::Computing(b)) if a == b)
                    {
                        dups.remote = next;
                    }
                }
                return;
            }
            result => {
                loader.active = None;
                if current {
                    dups.remote = match result {
                        Ok(Ok(groups)) => {
                            if groups.generations.is_some()
                                && let Some((scope, _)) = &request
                            {
                                loader.cached = Some((scope.clone(), Arc::clone(&groups)));
                            }
                            ViewState::Ready(groups)
                        }
                        Ok(Err((device, error))) => {
                            let error = if error == dups::GENERATION_CHANGED {
                                format!("{error}。「更新」を押してください")
                            } else {
                                error
                            };
                            ViewState::Failed(format!("{device}　{error}"))
                        }
                        Err(_) => ViewState::Failed("重複一覧の取得が中断されました".into()),
                    };
                } else {
                    loader.last = None;
                }
            }
        }
    }
    let Some(request) = request else {
        loader.last = None;
        if !matches!(dups.remote, ViewState::Local) {
            dups.remote = ViewState::Local;
        }
        return;
    };
    if loader.last.as_ref() == Some(&request) {
        return;
    }
    let scope = &request.0;
    let lan = lan.as_deref();
    let left = Device::requested(lan, scope.target.as_deref());
    let right = scope
        .comparison
        .as_ref()
        .map(|id| Device::requested(lan, id.as_deref()))
        .transpose();
    loader.last = Some(request.clone());
    let (left, right) = match (left, right) {
        (Ok(left), Ok(right)) => (left, right),
        (Err(error), _) | (_, Err(error)) => {
            dups.remote = ViewState::Failed(error);
            return;
        }
    };
    dups.remote = computing(scope, lan, None);
    let cached = loader
        .cached
        .as_ref()
        .filter(|(cached, _)| cached == scope)
        .map(|(_, groups)| Arc::clone(groups));
    let (send, receive) = channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let progress = Progress::default();
    let worker_progress = Arc::clone(&progress);
    match std::thread::Builder::new()
        .name("lisa-remote-dups".into())
        .spawn(move || {
            let fetch = || {
                fetch(
                    &left,
                    right.as_ref(),
                    &worker_cancelled,
                    cached.as_ref(),
                    &worker_progress,
                )
            };
            // 相手の写しが失われたら一度だけ作り直す。
            let mut result = fetch();
            if matches!(&result, Err((_, error)) if error == dups::GENERATION_CHANGED)
                && !worker_cancelled.load(Ordering::Relaxed)
            {
                result = fetch();
            }
            let _ = send.send(result);
        }) {
        Ok(_) => {
            loader.active = Some(Active {
                request,
                cancelled,
                receiver: receive,
                progress,
            });
            if !occluded.0 {
                redraw.write(RequestRedraw);
            }
        }
        Err(error) => dups.remote = ViewState::Failed(error.to_string()),
    }
}

fn computing(scope: &Scope, lan: Option<&LanState>, progress: Option<&Progress>) -> ViewState {
    let progress = progress
        .and_then(|progress| progress.lock().ok().map(|progress| *progress))
        .unwrap_or_default();
    ViewState::Computing(device_names(scope, lan).into_iter().zip(progress).collect())
}

fn device_names(scope: &Scope, lan: Option<&LanState>) -> Vec<String> {
    std::iter::once(&scope.target)
        .chain(scope.comparison.iter())
        .map(|id| match id {
            Some(id) => lan
                .and_then(|lan| lan.saved_peer(id))
                .map_or_else(|| id.clone(), |peer| peer.name),
            None => "この端末".into(),
        })
        .collect()
}
