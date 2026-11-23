//! 依頼元が対象端末に渡す、保持側への中継許可。

use serde::{Deserialize, Serialize};

use crate::db::{self, Db};

use super::KeepReference;

pub const GRANT_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayAction {
    Inspect,
    Reserve,
    Renew,
    Release,
}

/// 保持項目と通信先は許可の記録から復元する。
#[derive(Clone, Serialize, Deserialize)]
pub struct RelayCall {
    pub target_device: String,
    pub request_id: String,
    pub counter: i64,
    pub action: RelayAction,
}

/// 秘密値と証明を履歴やデバッグ表示に出さない。
#[derive(Serialize, Deserialize)]
pub struct RelayProof {
    pub call: RelayCall,
    pub secret: String,
    pub signature: [u8; 32],
}

#[derive(Serialize, Deserialize)]
pub struct Route {
    pub requester_device: String,
    pub target_device: String,
    pub request_id: String,
    pub address: std::net::SocketAddr,
    pub secret: String,
}

pub const DISCOVERY_FILE: &str = "relay-endpoints.json";
const DISCOVERY_VALID_MS: i64 = 65_000;

pub fn address(data_dir: &std::path::Path, route: &Route) -> std::net::SocketAddr {
    let current = std::fs::read(data_dir.join(DISCOVERY_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok());
    current
        .as_ref()
        .and_then(|data| data.get(&route.requester_device))
        .filter(|peer| {
            peer["seen_at"].as_i64().is_some_and(|time| {
                (0..=DISCOVERY_VALID_MS).contains(&db::now_ms().saturating_sub(time))
            })
        })
        .and_then(|peer| peer["address"].as_str())
        .and_then(|value| value.parse().ok())
        .unwrap_or(route.address)
}

pub type Client = fn(&std::path::Path, &Route, &KeepReference, RelayAction) -> Result<(), String>;

/// 対象端末が依頼元に発行した既存のペアリング鍵で要求を署名する。
pub fn client_call(
    data_dir: &std::path::Path,
    route: &Route,
    expected: &KeepReference,
    action: RelayAction,
    mut exchange: impl FnMut(serde_json::Value) -> Result<serde_json::Value, String>,
) -> Result<(), String> {
    use serde_json::{Value, json};
    let config: Value =
        serde_json::from_slice(&std::fs::read(data_dir.join("lan.json")).map_err(display)?)
            .map_err(display)?;
    let key = config["peers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|peer| peer["device_id"].as_str() == Some(route.requester_device.as_str()))
        .and_then(|peer| peer["key"].as_str())
        .ok_or_else(invalid)?;
    let counter = exchange(
        json!({"counter_query": true, "target_device": route.target_device,
        "request_id": route.request_id, "secret": route.secret}),
    )?["counter"]
        .as_i64()
        .ok_or_else(invalid)?;
    let call = RelayCall {
        target_device: route.target_device.clone(),
        request_id: route.request_id.clone(),
        counter,
        action,
    };
    let signature = sign(&call, &route.secret, key)?;
    let wrapped = exchange(json!(RelayProof {
        call: call.clone(),
        signature,
        secret: route.secret.clone()
    }))?;
    let reply: SignedReply = serde_json::from_value(wrapped).map_err(|_| invalid())?;
    if blake3::Hash::from_bytes(reply.signature)
        != blake3::Hash::from_bytes(sign_reply(&call, &route.secret, &reply.result, key)?)
    {
        return Err(invalid());
    }
    let result = reply.result;
    if action == RelayAction::Release {
        return if result["released"] == true {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    let observed = if action == RelayAction::Inspect {
        result.clone()
    } else {
        if result["expires_at"].as_i64().is_none() {
            return Err(invalid());
        }
        result["keep"].clone()
    };
    if serde_json::from_value::<KeepReference>(observed)
        .ok()
        .as_ref()
        != Some(expected)
    {
        return Err("中継先の保持項目が計画と異なります。重複を再取得してください".into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct SignedReply {
    pub result: serde_json::Value,
    pub signature: [u8; 32],
}

pub fn sign_reply(
    call: &RelayCall,
    secret: &str,
    result: &serde_json::Value,
    key: &str,
) -> Result<[u8; 32], String> {
    let payload = serde_json::to_vec(&(call, secret, result)).map_err(display)?;
    let key = blake3::derive_key("Lisa keep relay response v1", key.as_bytes());
    Ok(*blake3::keyed_hash(&key, &payload).as_bytes())
}

pub fn sign_keeper_reply(
    nonce: &str,
    tool: &str,
    result: &serde_json::Value,
    key: &str,
) -> Result<[u8; 32], String> {
    let payload = serde_json::to_vec(&(nonce, tool, result)).map_err(display)?;
    let key = blake3::derive_key("Lisa keeper response v1", key.as_bytes());
    Ok(*blake3::keyed_hash(&key, &payload).as_bytes())
}

pub fn verify_keeper_reply(
    nonce: &str,
    tool: &str,
    reply: SignedReply,
    key: &str,
) -> Result<serde_json::Value, String> {
    if blake3::Hash::from_bytes(reply.signature)
        != blake3::Hash::from_bytes(sign_keeper_reply(nonce, tool, &reply.result, key)?)
    {
        return Err("残す側の応答を確認できません".into());
    }
    Ok(reply.result)
}

pub struct RelayPermission {
    pub keep: KeepReference,
    pub lease: Option<String>,
}

pub fn issue(
    db: &Db,
    target: &str,
    request: uuid::Uuid,
    keep: &KeepReference,
) -> Result<String, String> {
    if target.is_empty() || target == keep.device_id || keep.chain.len() < 2 {
        return Err("中継許可の対象端末か保持項目が正しくありません".into());
    }
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let hash = blake3::hash(secret.as_bytes());
    let keep = serde_json::to_string(keep).map_err(display)?;
    let now = db::now_ms();
    let tx = db.conn().unchecked_transaction().map_err(display)?;
    tx.execute("DELETE FROM keep_relay_grant WHERE expires_at <= ?1", [now])
        .map_err(display)?;
    tx.execute(
        "INSERT INTO keep_relay_grant(target_device, request_id, secret_hash, keep, expires_at, counter)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![target, request.to_string(), hash.as_bytes().as_slice(), keep, now + GRANT_LIFETIME_MS],
    ).map_err(display)?;
    tx.commit().map_err(display)?;
    Ok(secret)
}

pub fn revoke(db: &Db, target: &str, request: uuid::Uuid) -> Result<(), String> {
    db.conn()
        .execute(
            "UPDATE keep_relay_grant SET revoked = 1 WHERE target_device = ?1 AND request_id = ?2",
            rusqlite::params![target, request.to_string()],
        )
        .map_err(display)?;
    Ok(())
}

/// ペアリング鍵の所有と、許可に結び付いた操作・通番を証明する。
pub fn sign(call: &RelayCall, secret: &str, pairing_key: &str) -> Result<[u8; 32], String> {
    let signed = serde_json::to_vec(&(call, secret)).map_err(display)?;
    let key = blake3::derive_key("Lisa keep relay pairing proof v1", pairing_key.as_bytes());
    Ok(*blake3::keyed_hash(&key, &signed).as_bytes())
}

pub fn next_counter(db: &Db, target: &str, request: &str, secret: &str) -> Result<i64, String> {
    let hash = blake3::hash(secret.as_bytes());
    db.conn()
        .query_row(
            "SELECT counter FROM keep_relay_grant
         WHERE target_device = ?1 AND request_id = ?2 AND secret_hash = ?3
           AND expires_at > ?4 AND revoked = 0",
            rusqlite::params![target, request, hash.as_bytes().as_slice(), db::now_ms()],
            |row| row.get::<_, i64>(0),
        )
        .ok()
        .and_then(|counter| counter.checked_add(1))
        .ok_or_else(invalid)
}

pub fn authorize(
    db: &mut Db,
    proof: &RelayProof,
    pairing_key: &str,
) -> Result<RelayPermission, String> {
    if pairing_key.is_empty()
        || blake3::Hash::from_bytes(sign(&proof.call, &proof.secret, pairing_key)?)
            != blake3::Hash::from_bytes(proof.signature)
    {
        return Err(invalid());
    }
    let hash = blake3::hash(proof.secret.as_bytes());
    let now = db::now_ms();
    let tx = db
        .conn_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(display)?;
    let row = tx
        .query_row(
            "UPDATE keep_relay_grant SET counter = ?1
         WHERE target_device = ?2 AND request_id = ?3 AND secret_hash = ?4
           AND counter = ?1 - 1 AND expires_at > ?5 AND revoked = 0
         RETURNING keep, lease",
            rusqlite::params![
                proof.call.counter,
                proof.call.target_device,
                proof.call.request_id,
                hash.as_bytes().as_slice(),
                now
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(|error| {
            if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                invalid()
            } else {
                display(error)
            }
        })?;
    let keep = serde_json::from_str(&row.0).map_err(display)?;
    tx.commit().map_err(display)?;
    Ok(RelayPermission { keep, lease: row.1 })
}

/// 失効・取り消し・別操作が先行した場合は、呼び出し側が新規の予約を解放する。
pub fn write_lease(db: &Db, call: &RelayCall, lease: Option<&str>) -> Result<(), String> {
    let changed = db
        .conn()
        .execute(
            "UPDATE keep_relay_grant SET lease = ?1
         WHERE target_device = ?2 AND request_id = ?3 AND counter = ?4
           AND expires_at > ?5 AND revoked = 0",
            rusqlite::params![
                lease,
                call.target_device,
                call.request_id,
                call.counter,
                db::now_ms()
            ],
        )
        .map_err(display)?;
    if changed != 1 {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> String {
    "中継許可が無効です。依頼元の Lisa を起動し、重複を再取得して確認してください".into()
}
fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dups::{COMPARISON_FORMAT, ContentKey};

    #[test]
    fn grant_rejects_replay_other_targets_other_requests_and_revocation() {
        let mut db = Db::open_in_memory().unwrap();
        let id = uuid::Uuid::new_v4();
        let keep = KeepReference {
            device_id: "keep".into(),
            drive_id: "drive".into(),
            chain: vec![1, 2],
            format: COMPARISON_FORMAT,
            key: ContentKey {
                directory: false,
                size: 1,
                hash: [1; 32],
            },
            identity: [1, 2],
        };
        let secret = issue(&db, "target", id, &keep).unwrap();
        assert!(issue(&db, "target", id, &keep).is_err());
        let mut call = RelayCall {
            target_device: "target".into(),
            request_id: id.to_string(),
            counter: 1,
            action: RelayAction::Reserve,
        };
        let proof = |call: &RelayCall, key| RelayProof {
            call: call.clone(),
            secret: secret.clone(),
            signature: sign(call, &secret, key).unwrap(),
        };
        let key = "fixture-pair-key";
        let first = proof(&call, key);
        assert!(authorize(&mut db, &first, "other-key").is_err());
        let mut tampered = proof(&call, key);
        tampered.call.action = RelayAction::Release;
        assert!(authorize(&mut db, &tampered, key).is_err());
        let permitted = authorize(&mut db, &first, key).unwrap();
        assert_eq!(permitted.keep, keep);
        assert!(authorize(&mut db, &first, key).is_err());
        call.counter = 2;
        call.target_device = "other".into();
        assert!(authorize(&mut db, &proof(&call, key), key).is_err());
        call.target_device = "target".into();
        call.request_id = uuid::Uuid::new_v4().to_string();
        assert!(authorize(&mut db, &proof(&call, key), key).is_err());
        call.request_id = id.to_string();
        assert_eq!(
            next_counter(&db, "target", &call.request_id, &secret).unwrap(),
            2
        );
        revoke(&db, "target", id).unwrap();
        assert!(authorize(&mut db, &proof(&call, key), key).is_err());
    }
}
