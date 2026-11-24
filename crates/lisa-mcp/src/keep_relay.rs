//! 既存の片方向ペアリングを使い、保持項目に限定した中継をする。

use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    time::Duration,
};

use lisa_core::merged::links;
use lisa_core::organize::remote_keep::{
    KeepReference,
    relay::{self, RelayAction, RelayProof},
};
use lisa_core::organize::{TransferAction, TransferReply, TransferRequest};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Auth, Server, err};

#[derive(Deserialize)]
struct Endpoint {
    device_id: String,
    addr: IpAddr,
    port: u16,
    key: String,
}

fn peer(data_dir: &Path, id: &str) -> Result<Endpoint, String> {
    let bytes = std::fs::read(data_dir.join("peers.json")).map_err(err)?;
    serde_json::from_slice::<Vec<Endpoint>>(&bytes)
        .map_err(err)?
        .into_iter()
        .find(|peer| peer.device_id == id && !peer.key.is_empty() && peer.port != 0)
        .ok_or_else(|| "中継先の端末が見つかりません。ペアリングと接続を確認してください".into())
}

impl Server {
    pub(super) fn keep_relay_grant(
        &self,
        tool: &str,
        args: &Value,
        auth: &Auth,
    ) -> Result<Value, String> {
        if !matches!(auth, Auth::Stdio) {
            return Err("中継許可の発行と取り消しは依頼元のローカル操作からだけ使えます".into());
        }
        let target = field(args, "target_device")?;
        let request = uuid::Uuid::parse_str(field(args, "request_id")?).map_err(err)?;
        let db = self.db()?;
        if tool == "lisa_keep_relay_revoke" {
            relay::revoke(&db, target, request)?;
            return Ok(json!({"revoked": true}));
        }
        peer(&self.data_dir, target)?;
        let keep: KeepReference =
            serde_json::from_value(args.get("keep").cloned().ok_or("keep が要ります")?)
                .map_err(err)?;
        let observed: KeepReference = serde_json::from_value(self.call_keeper(
            &db,
            &keep.device_id,
            target,
            "lisa_keep_inspect",
            json!({"keep":keep}),
        )?)
        .map_err(err)?;
        if observed.device_id != keep.device_id
            || observed.drive_id != keep.drive_id
            || observed.chain != keep.chain
            || observed.format != keep.format
            || observed.key != keep.key
            || (keep.identity != [0; 2] && observed.identity != keep.identity)
        {
            return Err("保持項目の参照が変わりました。重複を再取得して確認してください".into());
        }
        if args["dry_run"] == true {
            return Ok(json!({"keep": observed}));
        }
        let secret = relay::issue(&db, target, request, &observed)?;
        Ok(json!({"secret": secret, "keep": observed}))
    }

    fn call_keeper(
        &self,
        db: &lisa_core::db::Db,
        keeper: &str,
        target: &str,
        tool: &str,
        args: Value,
    ) -> Result<Value, String> {
        if db.meta("this_device").map_err(err)?.as_deref() == Some(keeper) {
            return self.duplicate_keep(
                tool,
                &args,
                &Auth::Peer {
                    device_id: target.into(),
                    name: String::new(),
                    remote_ip: None,
                },
            );
        }
        call_keep(&peer(&self.data_dir, keeper)?, tool, args)
    }

    pub(super) fn handle_keep_relay(&self, body: &[u8]) -> Result<Value, String> {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| "中継要求の形式が正しくありません")?;
        if let Some(proof) = value.get("transfer") {
            let proof: TransferProof = serde_json::from_value(proof.clone())
                .map_err(|_| "転送要求の書式が正しくありません")?;
            let owner = peer(&self.data_dir, &proof.request.marker.target.device_id)?;
            if !relay::verify_transfer::<TransferReply>(
                &proof.request,
                None,
                &owner.key,
                proof.signature,
            )? {
                return Err("転送要求の署名を確認できません".into());
            }
            let reply = self.forward_transfer(&proof.request)?;
            let signature = relay::sign_transfer(&proof.request, Some(&reply), &owner.key)?;
            return Ok(json!({ "reply": reply, "signature": signature }));
        }
        if let Some(proof) = value.get("link_proof") {
            let proof = serde_json::from_value::<relay::LinkProof>(proof.clone())
                .map_err(|_| "リンク中継要求の書式が正しくありません")?;
            return self.handle_link_relay(proof);
        }
        let mut db = self.db()?;
        if value.get("counter_query") == Some(&Value::Bool(true)) {
            let counter = relay::next_counter(
                &db,
                field(&value, "target_device")?,
                field(&value, "request_id")?,
                field(&value, "secret")?,
            )?;
            return Ok(json!({"counter": counter}));
        }
        let proof: RelayProof =
            serde_json::from_value(value).map_err(|_| "中継要求の形式が正しくありません")?;
        let target = peer(&self.data_dir, &proof.call.target_device)?;
        let permitted = relay::authorize(&mut db, &proof, &target.key)?;
        let keeper = |tool, args| {
            self.call_keeper(
                &db,
                &permitted.keep.device_id,
                &proof.call.target_device,
                tool,
                args,
            )
        };
        let result = (|| match proof.call.action {
            RelayAction::Inspect => keeper("lisa_keep_inspect", json!({"keep": permitted.keep})),
            RelayAction::Reserve => {
                if let Some(token) = permitted.lease {
                    if let Ok(result) = keeper("lisa_keep_renew", json!({"token":token})) {
                        relay::write_lease(&db, &proof.call, Some(&token))?;
                        return Ok(
                            json!({"expires_at": result["expires_at"],"keep":permitted.keep,"location":result["location"]}),
                        );
                    }
                    let _ = keeper("lisa_keep_release", json!({"token":token}));
                    relay::write_lease(&db, &proof.call, None)?;
                }
                let result = keeper("lisa_keep_reserve", json!({"keep": permitted.keep}))?;
                let token = field(&result, "token")?;
                if let Err(error) = relay::write_lease(&db, &proof.call, Some(token)) {
                    let _ = keeper("lisa_keep_release", json!({"token": token}));
                    return Err(error);
                }
                Ok(
                    json!({"expires_at": result["expires_at"],"keep":permitted.keep,"location":result["location"]}),
                )
            }
            RelayAction::Renew | RelayAction::Release => {
                let token = permitted
                    .lease
                    .ok_or("この依頼には有効な予約がありません")?;
                let renew = proof.call.action == RelayAction::Renew;
                let tool = if renew {
                    "lisa_keep_renew"
                } else {
                    "lisa_keep_release"
                };
                let result = keeper(tool, json!({"token": token}));
                if renew {
                    let result = result?;
                    relay::write_lease(&db, &proof.call, Some(&token))?;
                    Ok(
                        json!({"expires_at": result["expires_at"],"keep":permitted.keep,"location":result["location"]}),
                    )
                } else {
                    relay::write_lease(&db, &proof.call, None)?;
                    result.map(|_| json!({"released": true}))
                }
            }
        })()?;
        let signature = relay::sign_reply(&proof.call, &proof.secret, &result, &target.key)?;
        Ok(json!(relay::SignedReply { result, signature }))
    }
    fn forward_transfer(&self, request: &TransferRequest) -> Result<TransferReply, String> {
        let mut db = self.db()?;
        let now = lisa_core::db::now_ms();
        if !(0..=120_000).contains(&request.expires_at.saturating_sub(now)) {
            return Err("転送許可の期限が切れています".into());
        }
        if matches!(request.action, TransferAction::AddSource { .. }) {
            return Err("新しい実体の逆引きは中継で確かめてから登録します".into());
        }
        let repoint = match &request.action {
            TransferAction::Repoint { chosen } => {
                let (published, generation) = links::transfer_target(&db, &request.operation_id)?;
                if &published != chosen
                    || published.target != request.marker.target
                    || published.hash != request.marker.hash
                {
                    return Err("同じ実体と操作の戻し先を確認できません".into());
                }
                if links::relocation_repointed(&db, &request.marker, &request.operation_id)? {
                    return Ok(TransferReply {
                        published: true,
                        received: 0,
                        offsets: Vec::new(),
                    });
                }
                Some((published, generation))
            }
            _ => None,
        };
        let begin = matches!(
            request.action,
            TransferAction::Begin { .. } | TransferAction::Inspect | TransferAction::Repoint { .. }
        );
        let retire = matches!(request.action, TransferAction::Retire);
        if links::relocation_permitted(
            &db,
            &request.marker,
            request.generation,
            &request.operation_id,
            begin,
            retire,
        )? == links::Authorization::AlreadyApplied
        {
            return Ok(TransferReply {
                published: true,
                received: request.marker.size,
                offsets: Vec::new(),
            });
        }
        let destination = peer(&self.data_dir, &request.marker.source.device_id)?;
        let result = call_keep(
            &destination,
            "lisa_merged_receive_transfer",
            json!({ "request": request }),
        )?;
        let reply: TransferReply = serde_json::from_value(result).map_err(err)?;
        if let Some((published, generation)) = repoint {
            if !reply.published {
                return Err("リンクの付け替えを確認できません".into());
            }
            let mut updated = request.marker.clone();
            updated.target = published.source.clone();
            let keeper = peer(&self.data_dir, &published.source.device_id)?;
            let register = TransferRequest {
                operation_id: request.operation_id.clone(),
                marker: published,
                generation,
                expires_at: request.expires_at,
                action: TransferAction::AddSource {
                    source: updated.clone(),
                    generation: request.generation + 1,
                },
            };
            let recorded = call_keep(
                &keeper,
                "lisa_merged_receive_transfer",
                json!({ "request": register }),
            )?;
            if recorded["published"] != true {
                return Err("戻し先で新しいリンク元を確認できません".into());
            }
            links::finish_repoint(
                &mut db,
                &request.marker,
                &updated,
                request.generation,
                &request.operation_id,
            )?;
        } else {
            links::save_transfer_progress(
                &db,
                &request.operation_id,
                reply.received,
                request.marker.size,
            )?;
            if retire && reply.published {
                links::relocation_retired(&db, &request.marker, &request.operation_id)?;
            } else if reply.published {
                links::remember_transfer_target(
                    &db,
                    &request.operation_id,
                    &request.marker,
                    request.generation,
                )?;
            }
        }
        Ok(reply)
    }

    fn handle_link_relay(&self, proof: relay::LinkProof) -> Result<Value, String> {
        let mut db = self.db()?;
        let update = &proof.update;
        let marker = &update.marker;
        let source = peer(&self.data_dir, &marker.source.device_id)?;
        let request = &proof.request_id;
        let secret = &proof.secret;
        if !relay::verify_link_signature(
            request,
            update,
            secret,
            &source.key,
            false,
            proof.signature,
        )? {
            return Err("リンク中継の署名を確認できません".into());
        }
        let local_target =
            db.meta("this_device").map_err(err)?.as_deref() == Some(&marker.target.device_id);
        let relay_device = db
            .meta("this_device")
            .map_err(err)?
            .ok_or("中継端末 ID がありません")?;
        let forward = |db: &mut lisa_core::db::Db, token: Option<String>| {
            if local_target {
                let auth_device = if token.is_some() {
                    &marker.source.device_id
                } else {
                    &relay_device
                };
                self.apply_link_update(db, update, token.as_deref(), auth_device, &relay_device)
            } else {
                let keeper = peer(&self.data_dir, &marker.target.device_id)?;
                call_keep(
                    &keeper,
                    "lisa_merged_link_update",
                    json!({ "update": update, "token": token }),
                )
                .map(|_| ())
            }
        };
        if update.state == links::State::Reserved {
            let (keep, lease) = relay::link_reservation(&db, &proof)?;
            if keep.device_id != marker.target.device_id || keep.drive_id != marker.target.volume_id
            {
                return Err("リンクと予約対象が一致しません".into());
            }
            let persistent = relay::link_secret(secret, &marker.link_id);
            links::issue(&db, marker, request, &persistent)?;
            forward(&mut db, Some(lease))?;
        } else if links::authorize(&db, update, &source.device_id, secret)?
            == links::Authorization::Forward
        {
            forward(&mut db, None)?;
            links::forwarded(&db, update)?;
        }
        let signature = relay::sign_link(request, update, secret, &source.key, true)?;
        Ok(json!({ "signature": signature.as_bytes() }))
    }

    pub(super) fn link_relay_permit(
        &self,
        tool: &str,
        args: &Value,
        auth: &Auth,
    ) -> Result<Value, String> {
        if !matches!(auth, Auth::Stdio) {
            return Err("リンク中継許可の管理は依頼元のローカル操作からだけ使えます".into());
        }
        let db = self.db()?;
        if tool == "lisa_merged_link_revoke" {
            let id = uuid::Uuid::parse_str(field(args, "link_id")?).map_err(err)?;
            links::revoke(&db, id)?;
            Ok(json!({ "revoked": true }))
        } else {
            Ok(json!({ "permits": links::permits(&db)? }))
        }
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} が要ります"))
}

fn call_keep(endpoint: &Endpoint, tool: &str, mut arguments: Value) -> Result<Value, String> {
    let seconds = if tool == "lisa_merged_receive_transfer" {
        100
    } else {
        20
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(seconds)))
        .max_redirects(0)
        .build()
        .into();
    let address = SocketAddr::new(endpoint.addr, endpoint.port);
    let nonce = uuid::Uuid::new_v4().to_string();
    arguments["nonce"] = json!(nonce);
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
    let mut response = agent
        .post(format!("http://{address}/mcp"))
        .header("Authorization", &format!("Bearer {}", endpoint.key))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|_| "残す側に接続できません。重複を再取得して確認してください")?;
    let body = response.body_mut().read_to_string().map_err(err)?;
    let reply: Value = serde_json::from_str(&body).map_err(err)?;
    let result = &reply["result"];
    if let Some(value) = result.get("structuredContent") {
        let reply = serde_json::from_value(value.clone()).map_err(err)?;
        return relay::verify_keeper_reply(&nonce, tool, reply, &endpoint.key);
    }
    Err(result["content"][0]["text"]
        .as_str()
        .unwrap_or("残す側の検証結果がありません")
        .into())
}

pub fn relay_client(
    data_dir: &Path,
    route: &relay::Route,
    expected: &lisa_core::organize::remote_keep::KeepReference,
    action: RelayAction,
) -> Result<Option<lisa_core::merged::Location>, String> {
    relay::client_call(data_dir, route, expected, action, |body| {
        exchange(data_dir, route, body)
    })
}

#[derive(serde::Serialize, Deserialize)]
struct TransferProof {
    request: TransferRequest,
    signature: [u8; 32],
}

pub fn relocation_client(
    data_dir: &Path,
    relay_device: &str,
    request: &TransferRequest,
) -> Result<TransferReply, String> {
    let server = Server {
        data_dir: data_dir.to_path_buf(),
        cache_dir: None,
    };
    let is_relay_local =
        server.db()?.meta("this_device").map_err(err)?.as_deref() == Some(relay_device);
    if is_relay_local {
        return server.forward_transfer(request);
    }
    let config = super::load_lan_config(data_dir);
    let key = config
        .peers
        .iter()
        .find(|peer| peer.device_id == relay_device)
        .map(|peer| peer.key.as_str())
        .filter(|key| !key.is_empty())
        .ok_or("中継端末とペアリングされていません")?;
    // 応答は鍵で照合するので、発見を一度取りこぼしても最後に分かっている宛先で続ける。
    let address = relay::discovered_address(data_dir, relay_device)
        .or_else(|| relay::last_known_address(data_dir, relay_device))
        .ok_or("中継端末がオフラインです")?;
    let signature = relay::sign_transfer::<TransferReply>(request, None, key)?;
    let response = exchange_at(
        address,
        json!({ "transfer": { "request": request, "signature": signature } }),
    )?;
    let reply: TransferReply = serde_json::from_value(response["reply"].clone()).map_err(err)?;
    let signature = serde_json::from_value(response["signature"].clone()).map_err(err)?;
    if !relay::verify_transfer(request, Some(&reply), key, signature)? {
        return Err("転送の応答を確認できません".into());
    }
    Ok(reply)
}

pub fn link_relay_client(
    data_dir: &Path,
    route: &relay::Route,
    update: &links::Update,
) -> Result<(), String> {
    let config = super::load_lan_config(data_dir);
    let key = config
        .peers
        .iter()
        .find(|peer| peer.device_id == route.requester_device)
        .map(|peer| peer.key.as_str())
        .filter(|key| !key.is_empty())
        .ok_or("中継側とのペアリングがありません")?;
    let secret = if update.state == links::State::Reserved {
        route.secret.clone()
    } else {
        relay::link_secret(&route.secret, &update.marker.link_id)
    };
    let proof = relay::LinkProof {
        request_id: route.request_id.clone(),
        signature: *relay::sign_link(&route.request_id, update, &secret, key, false)?.as_bytes(),
        update: update.clone(),
        secret,
    };
    let response = exchange(data_dir, route, json!({ "link_proof": proof }))?;
    let signature: [u8; 32] = serde_json::from_value(response["signature"].clone())
        .map_err(|_| "リンク中継の応答が不正です")?;
    if !relay::verify_link_signature(
        &proof.request_id,
        update,
        &proof.secret,
        key,
        true,
        signature,
    )? {
        return Err("リンク中継の応答を確認できません".into());
    }
    Ok(())
}

fn exchange(data_dir: &Path, route: &relay::Route, body: Value) -> Result<Value, String> {
    exchange_at(relay::address(data_dir, route), body)
}

fn exchange_at(address: SocketAddr, body: Value) -> Result<Value, String> {
    // 付け替えの小分け（数 MiB を 2 段で中継）は、遅い回線でも待てるよう長めにする。
    let seconds = if body.get("transfer").is_some() {
        120
    } else {
        12
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(seconds)))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .post(format!("http://{address}/keep-relay"))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|_| "依頼元に接続できません")?;
    let reply: Value =
        serde_json::from_str(&response.body_mut().read_to_string().map_err(err)?).map_err(err)?;
    if reply.get("ok") == Some(&Value::Bool(true)) {
        return reply
            .get("result")
            .cloned()
            .ok_or("依頼元の応答がありません".into());
    }
    Err(reply["error"]
        .as_str()
        .unwrap_or("依頼元が中継を拒否しました")
        .into())
}

#[cfg(all(test, unix, feature = "verify-remote-keep"))]
mod tests;
