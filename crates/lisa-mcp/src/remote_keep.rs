//! ペアリングした端末から残す項目を短時間借りる。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use lisa_core::organize::remote_keep::{self, KEEP_LEASE_MS, KeepReference, ProtectedKeep};
use serde_json::{Value, json};

use super::{Auth, Server, err};

const MAX_RESERVATIONS: usize = 32;
type Reservations = HashMap<String, Arc<Mutex<Reservation>>>;
static RESERVATIONS: Mutex<Option<Reservations>> = Mutex::new(None);
static REAPER: OnceLock<()> = OnceLock::new();

struct Reservation {
    owner: String,
    data_dir: PathBuf,
    reference: KeepReference,
    deadline: Instant,
    prepared: ProtectedKeep,
}

impl Server {
    pub(super) fn signed_duplicate_keep(
        &self,
        tool: &str,
        args: &Value,
        auth: &Auth,
    ) -> Result<Value, String> {
        let Some(nonce) = args["nonce"].as_str() else {
            return self.duplicate_keep(tool, args, auth);
        };
        let Auth::Peer { device_id, .. } = auth else {
            return Err("残す項目の予約はペアリング済み端末からだけ使えます".into());
        };
        let config = super::load_lan_config(&self.data_dir);
        let key = config
            .peers
            .iter()
            .find(|peer| peer.device_id == *device_id)
            .map(|peer| peer.key.as_str())
            .filter(|key| !key.is_empty())
            .ok_or("ペアリング鍵を確認できません")?;
        let result = self.duplicate_keep(tool, args, auth)?;
        let signature = remote_keep::relay::sign_keeper_reply(nonce, tool, &result, key)?;
        Ok(json!(remote_keep::relay::SignedReply { result, signature }))
    }

    pub(super) fn duplicate_keep(
        &self,
        tool: &str,
        args: &Value,
        auth: &Auth,
    ) -> Result<Value, String> {
        let Auth::Peer { device_id, .. } = auth else {
            return Err("残す項目の予約はペアリング済み端末からだけ使えます".into());
        };
        let db = self.db()?;
        if tool == "lisa_keep_inspect" {
            let reference = parse_reference(args)?;
            let reference = ProtectedKeep::inspect(&db, reference)?;
            return Ok(json!(reference));
        }
        if tool == "lisa_keep_reserve" {
            let reference = parse_reference(args)?;
            let prepared = ProtectedKeep::open(&db, &reference)?;
            let token = uuid::Uuid::new_v4().to_string();
            let mut state = RESERVATIONS.lock().map_err(err)?;
            let state = state.get_or_insert_with(HashMap::new);
            if state.len() >= MAX_RESERVATIONS {
                return Err("残す項目の予約が混み合っています。再取得して確認してください".into());
            }
            start_reaper()?;
            let expires_at =
                remote_keep::register(&db, &self.data_dir, &reference, &token, &prepared)?;
            let deadline = Instant::now() + lease_duration();
            state.insert(
                token.clone(),
                Arc::new(Mutex::new(Reservation {
                    owner: device_id.clone(),
                    data_dir: self.data_dir.clone(),
                    reference,
                    deadline,
                    prepared,
                })),
            );
            return Ok(json!({"token": token, "expires_at": expires_at}));
        }

        let token = args
            .get("token")
            .and_then(Value::as_str)
            .ok_or("予約トークンが要ります")?;
        let lease = RESERVATIONS
            .lock()
            .map_err(err)?
            .as_ref()
            .and_then(|reservations| reservations.get(token))
            .cloned()
            .ok_or("予約が失効しました。重複を再取得して確認してください")?;
        let mut reservation = lease.lock().map_err(err)?;
        if reservation.owner != *device_id || reservation.data_dir != self.data_dir {
            return Err("この予約の依頼元が一致しません".into());
        }
        if tool == "lisa_keep_release" {
            remote_keep::release(&db, token)?;
            reservation.deadline = Instant::now();
            drop(reservation);
            if let Some(state) = RESERVATIONS.lock().map_err(err)?.as_mut() {
                state.remove(token);
            }
            return Ok(json!({"released": true}));
        }
        if Instant::now() >= reservation.deadline {
            return Err("予約が失効しました。重複を再取得して確認してください".into());
        }
        let Reservation {
            reference,
            prepared,
            ..
        } = &mut *reservation;
        prepared.verify_held(&db, reference)?;
        if Instant::now() >= reservation.deadline {
            return Err("検証中に予約が失効しました。重複を再取得して確認してください".into());
        }
        let expires_at = remote_keep::renew(&db, token)?;
        reservation.deadline = Instant::now() + lease_duration();
        Ok(json!({"token": token, "expires_at": expires_at}))
    }
}

fn parse_reference(args: &Value) -> Result<KeepReference, String> {
    serde_json::from_value(args.get("keep").cloned().ok_or("keep が要ります")?).map_err(err)
}

fn lease_duration() -> Duration {
    Duration::from_millis(KEEP_LEASE_MS as u64)
}

fn start_reaper() -> Result<(), String> {
    if REAPER.get().is_some() {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("lisa-keep-expiry".into())
        .spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let expired = {
                    let Ok(mut state) = RESERVATIONS.lock() else {
                        return;
                    };
                    let Some(state) = state.as_mut() else {
                        continue;
                    };
                    let expired: Vec<_> = state
                        .iter()
                        .filter_map(|(token, lease)| {
                            let data_dir = lease
                                .try_lock()
                                .ok()
                                .filter(|lease| Instant::now() >= lease.deadline)?
                                .data_dir
                                .clone();
                            Some((token.clone(), data_dir))
                        })
                        .collect();
                    for (token, _) in &expired {
                        state.remove(token);
                    }
                    expired
                };
                for (token, data_dir) in expired {
                    let _ = remote_keep::release_on_disk(&data_dir.join("lisa.db"), &token);
                }
            }
        })
        .map_err(err)?;
    let _ = REAPER.set(());
    Ok(())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use lisa_core::{
        db::Db,
        dups::{COMPARISON_FORMAT, ContentKey},
    };

    #[test]
    fn unsupported_keeper_refuses_even_a_paired_request() {
        let data_dir =
            std::env::temp_dir().join(format!("lisa-keep-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let mut db = Db::open(&data_dir.join("lisa.db")).unwrap();
        let device = db.this_device("旧PC", "旧PC", "fixture").unwrap();
        drop(db);
        let keep = KeepReference {
            device_id: device,
            drive_id: "fixture".into(),
            chain: vec![1, 2],
            format: COMPARISON_FORMAT,
            key: ContentKey {
                directory: false,
                size: 5,
                hash: [1; 32],
            },
            identity: [1, 2],
        };
        let server = Server {
            data_dir: data_dir.clone(),
            cache_dir: None,
        };
        let args = json!({"name":"lisa_keep_reserve","arguments":{"keep":keep}});
        let unauthorized = server.call(&args, &Auth::Token);
        assert_eq!(unauthorized["isError"], true);
        assert!(unauthorized.to_string().contains("ペアリング"));
        let auth = Auth::Peer {
            device_id: "fixture-peer".into(),
            name: "新PC".into(),
            remote_ip: None,
        };
        let denied = server.call(&args, &auth);
        assert_eq!(denied["isError"], true);
        assert!(denied.to_string().contains("比較表示のみ"));
        std::fs::remove_dir_all(data_dir).unwrap();
    }
}
