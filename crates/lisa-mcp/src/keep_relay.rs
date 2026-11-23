//! 既存の片方向ペアリングを使い、保持項目に限定した中継をする。

use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    time::Duration,
};

use lisa_core::organize::remote_keep::{
    KeepReference,
    relay::{self, RelayAction, RelayProof},
};
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
                            json!({"expires_at": result["expires_at"],"keep":permitted.keep}),
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
                Ok(json!({"expires_at": result["expires_at"],"keep":permitted.keep}))
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
                    Ok(json!({"expires_at": result["expires_at"],"keep":permitted.keep}))
                } else {
                    relay::write_lease(&db, &proof.call, None)?;
                    result.map(|_| json!({"released": true}))
                }
            }
        })()?;
        let signature = relay::sign_reply(&proof.call, &proof.secret, &result, &target.key)?;
        Ok(json!(relay::SignedReply { result, signature }))
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} が要ります"))
}

fn call_keep(endpoint: &Endpoint, tool: &str, mut arguments: Value) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
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
) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .max_redirects(0)
        .build()
        .into();
    relay::client_call(data_dir, route, expected, action, |body| {
        let mut response = agent
            .post(format!(
                "http://{}/keep-relay",
                relay::address(data_dir, route)
            ))
            .header("Content-Type", "application/json")
            .send(body.to_string().as_bytes())
            .map_err(|_| "依頼元に接続できません")?;
        let reply: Value =
            serde_json::from_str(&response.body_mut().read_to_string().map_err(err)?)
                .map_err(err)?;
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
    })
}

#[cfg(all(test, unix, feature = "verify-remote-keep"))]
mod tests;
