//! 重複パネルからの整理依頼と、依頼 ID を使った照会。

use bevy::{prelude::*, window::RequestRedraw};
use lisa_core::organize::{CreateResult, OpState, PlanState, remote_keep::KeepReference};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};

use crate::lan::{LanState, SavedPeer, call_mcp_tool};

#[derive(Clone, PartialEq)]
pub(crate) struct Item {
    pub device: Option<String>,
    pub reference: KeepReference,
    pub path: String,
    pub local_path: Option<std::path::PathBuf>,
}

#[derive(Default)]
pub(crate) struct Selection {
    pub keep: Option<usize>,
    pub sources: std::collections::BTreeSet<usize>,
}

impl Selection {
    pub fn choose_keep(&mut self, index: usize) {
        self.keep = Some(index);
        self.sources.remove(&index);
    }

    pub fn toggle_source(&mut self, index: usize) {
        if self.keep != Some(index) && !self.sources.remove(&index) {
            self.sources.insert(index);
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub id: String,
    pub target: String,
    pub device: String,
    pub paths: Vec<String>,
    pub result: Option<Value>,
    pub undo_requested: bool,
    #[serde(default)]
    pub connection_error: Option<String>,
    /// 依頼を送り始めた時刻（ミリ秒）。古い記録には無い。
    #[serde(default)]
    pub created_ms: Option<i64>,
    /// 作成応答が無いまま `UNREACHED_AFTER` が過ぎ、同じ依頼 ID の照会でも対象端末に無かった。
    #[serde(default)]
    pub unreached: bool,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// 作成応答が無い依頼を「未到達かもしれない」として照会し始めるまでの時間。
const UNREACHED_AFTER_MS: i64 = 2 * 60 * 1000;

impl Request {
    pub fn state(&self) -> Option<PlanState> {
        self.result.as_ref()?.get("plan")?["state"]
            .as_str()
            .and_then(PlanState::from_name)
    }

    fn has_isolated_items(&self) -> bool {
        self.result.as_ref().is_some_and(|value| {
            value["plan"]["purged_at"].is_null()
                && value["ops"]
                    .as_array()
                    .is_some_and(|ops| ops.iter().any(|op| op["state"] == OpState::Done.as_str()))
        })
    }

    pub fn can_undo(&self) -> bool {
        matches!(self.state(), Some(PlanState::Done | PlanState::Partial))
            && self.has_isolated_items()
            && !self.undo_requested
    }

    fn can_forget(&self) -> bool {
        self.unreached
            || self.state().is_some_and(PlanState::is_terminal) && !self.has_isolated_items()
    }

    /// 作成応答が無いまま十分に時間が経ち、対象端末に計画が無いと確認できた。
    pub fn can_discard(&self) -> bool {
        self.unreached
    }

    fn awaiting_creation(&self, now: i64) -> bool {
        self.result.is_none()
            && self
                .created_ms
                .is_none_or(|created| now.saturating_sub(created) >= UNREACHED_AFTER_MS)
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct Quote {
    pub revision: u64,
    pub keep: Item,
    pub sources: Vec<Item>,
    pub result: Result<(), String>,
}

#[derive(Resource, Default)]
pub(crate) struct Requests {
    pub entries: Vec<Request>,
    pub notice: Option<String>,
    pending: Option<Mutex<mpsc::Receiver<Update>>>,
    next_poll: Option<Instant>,
    loaded: bool,
    polling: bool,
    poll_index: usize,
    pub quote: Option<Quote>,
}

struct Update {
    entries: Vec<Request>,
    notice: Option<String>,
    refresh: Vec<String>,
    quote: Option<Quote>,
    submitted_revision: Option<u64>,
}

fn persist(dir: &Path, entries: &[Request]) -> Result<(), String> {
    let path = dir.join("duplicate-requests.json");
    let temp = path.with_extension("tmp");
    let bytes = serde_json::to_vec(entries).map_err(|err| err.to_string())?;
    std::fs::write(&temp, bytes)
        .and_then(|_| std::fs::rename(&temp, path))
        .map_err(|err| err.to_string())
}

fn local_tool(dir: &Path, name: &str, args: Value) -> Result<Value, String> {
    let server = lisa_mcp::Server {
        data_dir: dir.to_path_buf(),
        cache_dir: None,
    };
    let result = server.call(
        &json!({"name": name, "arguments": args}),
        &lisa_mcp::Auth::Stdio,
    );
    if result["isError"] == true {
        return Err(result["content"][0]["text"]
            .as_str()
            .unwrap_or("整理の確認に失敗しました")
            .into());
    }
    result
        .get("structuredContent")
        .cloned()
        .ok_or("整理の応答がありません".into())
}

/// 失敗しても許可は期限で消えるため、取り消しの失敗は通知しない。
fn revoke_grant(dir: &Path, target: &str, request: &str) {
    let _ = local_tool(
        dir,
        "lisa_keep_relay_revoke",
        json!({"target_device": target, "request_id": request}),
    );
}

fn call(peer: &SavedPeer, tool: &str, arguments: Value) -> Result<Value, String> {
    call_mcp_tool(&peer.addr, peer.port, &peer.key, tool, arguments).map_err(|(_, error)| error)
}

fn checked(value: Value) -> Result<(), String> {
    let result: CreateResult = serde_json::from_value(value).map_err(|err| err.to_string())?;
    if result.summary.refused == 0 {
        Ok(())
    } else {
        Err(refusal(&result))
    }
}

fn file_results_changed(before: Option<&Value>, after: &Value) -> bool {
    let changed = |previous: Option<&Value>, op: &Value| {
        matches!(op["state"].as_str(), Some("done" | "undone"))
            && previous.is_none_or(|previous| previous["state"] != op["state"])
    };
    after["ops"].as_array().is_some_and(|ops| {
        ops.iter()
            .enumerate()
            .any(|(index, op)| changed(before.and_then(|value| value["ops"].get(index)), op))
    })
}

fn refusal(result: &CreateResult) -> String {
    let reasons: Vec<_> = result
        .checks
        .iter()
        .filter_map(|check| check.check.strip_prefix("refused:"))
        .collect();
    if reasons.is_empty() {
        "整理計画を作れませんでした".into()
    } else {
        reasons.join("・")
    }
}

struct Submission {
    dir: std::path::PathBuf,
    target: Option<SavedPeer>,
    port: Option<u16>,
    keep: Item,
    sources: Vec<Item>,
    revision: u64,
    preview: bool,
}

impl Submission {
    fn execute(&self, entries: &mut Vec<Request>) -> Result<(), String> {
        let cross_device = self.keep.device != self.sources[0].device;
        if cross_device && self.target.is_none() {
            return Err("この端末が隔離対象の端末間整理は比較表示のみです".into());
        }

        let db = super::remote::open_db()?;
        let own_id = db
            .meta("this_device")
            .map_err(|err| err.to_string())?
            .ok_or("この端末の ID がありません")?;
        let reference = |item: &Item| {
            let mut reference = item.reference.clone();
            if let Some(path) = &item.local_path {
                let resolved = lisa_core::query::locate(&db, path)
                    .map_err(|err| err.to_string())?
                    .filter(|resolved| resolved.chain == reference.chain)
                    .ok_or("重複の所在を再取得してください")?;
                reference.drive_id = resolved.drive_id;
            }
            if item.device.is_none() {
                reference.device_id.clone_from(&own_id);
            }
            Ok::<_, String>(reference)
        };
        let keep = reference(&self.keep)?;
        let sources = self
            .sources
            .iter()
            .map(reference)
            .collect::<Result<Vec<_>, _>>()?;
        drop(db);

        let id = lisa_mcp::new_uuid();
        let paths: Vec<_> = self.sources.iter().map(|item| item.path.clone()).collect();
        if let Some(target) = &self.target
            && !self.preview
            && entries.iter().any(|entry| {
                entry.target == target.device_id
                    && entry.paths == paths
                    && !entry.unreached
                    && entry.state().is_none_or(|state| !state.is_terminal())
            })
        {
            return Err("この整理対象は依頼済みです。整理の履歴で状態を確認してください".into());
        }

        let mut args = json!({"sources": sources, "keep": keep, "dry_run": self.preview});
        let tool = if cross_device {
            let target = self.target.as_ref().expect("端末間の隔離対象を確認済み");
            let port = self.port.ok_or("依頼元の LAN 公開を有効にしてください")?;
            let grant = local_tool(
                &self.dir,
                "lisa_keep_relay_issue",
                json!({
                    "target_device": target.device_id, "request_id": id,
                    "keep": keep, "dry_run": self.preview,
                }),
            )?;
            let prepared = (|| {
                let protected = call(
                    target,
                    "lisa_duplicate_sources",
                    json!({"sources": sources}),
                )?;
                let keep: KeepReference =
                    serde_json::from_value(grant["keep"].clone()).map_err(|err| err.to_string())?;
                let sources: Vec<KeepReference> =
                    serde_json::from_value(protected["sources"].clone())
                        .map_err(|err| err.to_string())?;
                let mut identities = std::collections::HashSet::from([keep.identity]);
                if sources
                    .iter()
                    .any(|source| source.identity == [0; 2] || !identities.insert(source.identity))
                {
                    return Err(
                        "同じ中身で独立した実体だと確認できません。比較表示のみ".to_string()
                    );
                }
                Ok((keep, sources))
            })();
            let (keep, sources) = match prepared {
                Ok(prepared) if !self.preview => prepared,
                other => {
                    // 依頼が作られない経路では、発行した中継許可を残さない。
                    revoke_grant(&self.dir, &target.device_id, &id);
                    return other.map(|_| ());
                }
            };
            args["keep"] = json!(keep);
            args["sources"] = json!(sources);
            args["relay_secret"] = grant["secret"].clone();
            args["relay_port"] = json!(port);
            args["keep_label"] = json!(self.keep.path);
            "lisa_duplicate_plan_create"
        } else {
            "lisa_duplicate_local_plan"
        };

        let Some(target) = &self.target else {
            let result = local_tool(&self.dir, tool, args)?;
            return if self.preview {
                checked(result)
            } else {
                created(result)
            };
        };
        if self.preview {
            return checked(call(target, tool, args)?);
        }

        args["request_id"] = json!(id);
        self.send(
            target,
            tool,
            args,
            Request {
                id,
                target: target.device_id.clone(),
                device: target.name.clone(),
                paths,
                result: None,
                undo_requested: false,
                connection_error: None,
                created_ms: Some(now_ms()),
                unreached: false,
            },
            entries,
        )
    }

    fn send(
        &self,
        target: &SavedPeer,
        tool: &str,
        args: Value,
        request: Request,
        entries: &mut Vec<Request>,
    ) -> Result<(), String> {
        let mut saved = entries.clone();
        if saved.len() >= 100 {
            let index = saved
                .iter()
                .rposition(Request::can_forget)
                .ok_or("追跡中の整理依頼が上限に達しました")?;
            saved.remove(index);
        }
        saved.insert(0, request);
        if let Err(err) = persist(&self.dir, &saved) {
            revoke_grant(&self.dir, &target.device_id, &saved[0].id);
            return Err(err);
        }
        *entries = saved;
        // 通信断や不明な応答では依頼 ID を残し、作成を再送せず状態を照会する。
        let result = call(target, tool, args)
            .and_then(|reply| {
                serde_json::from_value::<CreateResult>(reply).map_err(|err| err.to_string())
            })
            .map_err(|err| {
                format!("依頼の結果を確認できません。整理の履歴で同じ依頼を照会します。{err}")
            })?;
        if result.created {
            Ok(())
        } else {
            let request = entries.remove(0);
            revoke_grant(&self.dir, &request.target, &request.id);
            persist(&self.dir, entries)?;
            Err(refusal(&result))
        }
    }
}

fn created(value: Value) -> Result<(), String> {
    let result: CreateResult = serde_json::from_value(value).map_err(|err| err.to_string())?;
    if result.created {
        Ok(())
    } else {
        Err(refusal(&result))
    }
}

impl Requests {
    pub fn busy(&self) -> bool {
        self.pending.is_some() && !self.polling
    }

    fn run(&mut self, work: impl FnOnce(Vec<Request>) -> Update + Send + 'static) {
        let (send, receive) = mpsc::channel();
        let entries = self.entries.clone();
        self.pending = Some(Mutex::new(receive));
        std::thread::spawn(move || {
            let _ = send.send(work(entries));
        });
    }

    pub fn submit(
        &mut self,
        lan: Option<&LanState>,
        keep: Item,
        sources: Vec<Item>,
        revision: u64,
        preview: bool,
    ) {
        if self.pending.is_some() || !self.loaded || sources.is_empty() {
            return;
        }
        if sources
            .iter()
            .any(|source| source.device != sources[0].device)
        {
            self.notice = Some("整理対象は一つの端末から選んでください".into());
            return;
        }
        let Some(dir) = crate::scan::data_dir() else {
            return;
        };
        let target = sources[0]
            .device
            .as_ref()
            .and_then(|id| lan.and_then(|lan| lan.saved_peer(id)));
        if sources[0].device.is_some() && target.is_none() {
            self.notice = Some("対象端末のペアリングを確認してください".into());
            return;
        }
        let port = lan.and_then(LanState::relay_port);
        self.quote = None;
        self.notice = Some("整理計画を確認中…".into());
        let submission = Submission {
            dir,
            target,
            port,
            keep,
            sources,
            revision,
            preview,
        };
        self.run(move |mut entries| {
            let outcome = submission.execute(&mut entries);
            if submission.preview {
                let quote = Quote {
                    revision: submission.revision,
                    keep: submission.keep,
                    sources: submission.sources,
                    result: outcome,
                };
                return Update {
                    entries,
                    notice: None,
                    refresh: Vec::new(),
                    quote: Some(quote),
                    submitted_revision: None,
                };
            }
            let submitted_revision = outcome.is_ok().then_some(submission.revision);
            let notice = match outcome {
                Ok(()) => "対象端末での許可を待っています".into(),
                Err(err) => err,
            };
            Update {
                entries,
                notice: Some(notice),
                refresh: Vec::new(),
                quote: None,
                submitted_revision,
            }
        });
    }

    /// 未到達と確認した依頼を手元の追跡から外す。
    pub fn discard(&mut self, id: &str) {
        if self.pending.is_some() {
            return;
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == id && entry.can_discard())
        else {
            return;
        };
        let (target, id) = (entry.target.clone(), entry.id.clone());
        let Some(dir) = crate::scan::data_dir() else {
            return;
        };
        self.run(move |mut entries| {
            entries.retain(|entry| entry.id != id);
            revoke_grant(&dir, &target, &id);
            let notice = persist(&dir, &entries)
                .err()
                .or(Some("未到達の依頼を手元から破棄しました".into()));
            Update {
                entries,
                notice,
                refresh: Vec::new(),
                quote: None,
                submitted_revision: None,
            }
        });
    }

    pub fn undo(&mut self, lan: Option<&LanState>, id: &str) {
        if self.pending.is_some() {
            return;
        }
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.id == id && entry.can_undo())
        else {
            return;
        };
        let Some(peer) = lan.and_then(|lan| lan.saved_peer(&entry.target)) else {
            self.notice = Some("対象端末のペアリングを確認してください".into());
            return;
        };
        let id = id.to_owned();
        self.run(move |mut entries| {
            let result = call(&peer, "lisa_plan_request_undo", json!({"request_id": id}));
            let notice = match result {
                Ok(_) => {
                    if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
                        entry.undo_requested = true;
                    }
                    "対象端末での復元確認を待っています".into()
                }
                Err(err) => err,
            };
            Update {
                entries,
                notice: Some(notice),
                refresh: Vec::new(),
                quote: None,
                submitted_revision: None,
            }
        });
    }
}

pub(crate) fn update(
    mut requests: ResMut<Requests>,
    mut dups: ResMut<super::Dups>,
    lan: Option<Res<LanState>>,
    menus: Res<crate::organize::OrganizeState>,
    mut remote_refresh: ResMut<crate::lan::RemoteRefresh>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if let Some(receiver) = &requests.pending {
        let update = receiver.lock().ok().and_then(|r| match r.try_recv() {
            Ok(update) => Some(update),
            Err(mpsc::TryRecvError::Disconnected) => Some(Update {
                entries: requests.entries.clone(),
                notice: Some("整理の確認が中断されました".into()),
                refresh: Vec::new(),
                quote: None,
                submitted_revision: None,
            }),
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some(update) = update {
            requests.pending = None;
            requests.polling = false;
            if requests.entries != update.entries {
                requests.entries = update.entries;
            }
            if update.submitted_revision == Some(dups.refresh) {
                dups.selection = Default::default();
            }
            if let Some(quote) = update.quote {
                requests.quote = Some(quote);
                requests.notice = None;
            }
            if update.notice.is_some() || !update.refresh.is_empty() {
                requests.notice = update.notice;
            }
            if !update.refresh.is_empty() {
                if update.refresh.iter().any(|device| {
                    dups.scope.target.as_ref() == Some(device)
                        || dups.scope.comparison.as_ref().and_then(Option::as_ref) == Some(device)
                }) {
                    dups.invalidate();
                }
                remote_refresh.0.extend(update.refresh);
            }
        }
        redraw.write(RequestRedraw);
        return;
    }
    let Some(dir) = crate::scan::data_dir() else {
        return;
    };
    if !requests.loaded {
        requests.loaded = true;
        requests.run(
            move |entries| match std::fs::read(dir.join("duplicate-requests.json")) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(entries) => Update {
                        entries,
                        notice: None,
                        refresh: Vec::new(),
                        quote: None,
                        submitted_revision: None,
                    },
                    Err(err) => Update {
                        entries,
                        notice: Some(err.to_string()),
                        refresh: Vec::new(),
                        quote: None,
                        submitted_revision: None,
                    },
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Update {
                    entries,
                    notice: None,
                    refresh: Vec::new(),
                    quote: None,
                    submitted_revision: None,
                },
                Err(err) => Update {
                    entries,
                    notice: Some(err.to_string()),
                    refresh: Vec::new(),
                    quote: None,
                    submitted_revision: None,
                },
            },
        );
        redraw.write(RequestRedraw);
        return;
    }
    if (lan.is_none() || requests.entries.is_empty())
        || (!dups.open && !menus.panel_open)
        || requests.next_poll.is_some_and(|next| Instant::now() < next)
    {
        return;
    }
    requests.next_poll = Some(Instant::now() + Duration::from_secs(2));
    let peers = lan
        .as_ref()
        .map(|lan| {
            lan.paired_devices()
                .filter_map(|(id, _)| lan.saved_peer(id))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    requests.polling = true;
    let start = requests.poll_index % requests.entries.len();
    requests.poll_index = (start + 1) % requests.entries.len();
    requests.run(move |mut entries| {
        let mut refresh = Vec::new();
        let total = entries.len();
        let now = now_ms();
        for offset in 0..total {
            let entry = &mut entries[(start + offset) % total];
            let Some(peer) = peers.iter().find(|peer| peer.device_id == entry.target) else {
                continue;
            };
            let before = entry.result.as_ref();
            if before.is_some_and(|value| {
                matches!(
                    value["plan"]["state"].as_str(),
                    Some("rejected" | "expired" | "cancelled" | "undone")
                ) || value["plan"]["purged_at"].is_number()
            }) {
                continue;
            }
            let silent = entry.awaiting_creation(now);
            match call(
                peer,
                "lisa_plan_status",
                json!({"request_id": entry.id, "ops_limit": lisa_core::organize::MAX_OPS}),
            ) {
                Ok(after) => {
                    let reconnected = entry.connection_error.take().is_some();
                    // 見つかった依頼は、再送せずそのまま通常の追跡に戻す。
                    entry.unreached = false;
                    if reconnected || file_results_changed(before, &after) {
                        refresh.push(entry.target.clone());
                    }
                    entry.undo_requested = after["plan"]["restore_requested"] == true;
                    let was_open =
                        before.is_none() || entry.state().is_some_and(|s| !s.is_terminal());
                    entry.result = Some(after);
                    if was_open && entry.state().is_some_and(PlanState::is_terminal) {
                        revoke_grant(&dir, &entry.target, &entry.id);
                    }
                }
                Err(err) if silent && err.contains(lisa_mcp::NO_PLAN_FOR_REQUEST) => {
                    entry.connection_error = None;
                    if !entry.unreached {
                        entry.unreached = true;
                        revoke_grant(&dir, &entry.target, &entry.id);
                    }
                }
                Err(err) => entry.connection_error = Some(err),
            }
            break;
        }
        let notice = persist(&dir, &entries).err();
        Update {
            entries,
            notice,
            refresh,
            quote: None,
            submitted_revision: None,
        }
    });
    redraw.write(RequestRedraw);
}
