//! The carrier (iroh gossip) through an elastOS node's Runtime API, as a native
//! process on that node uses it: attach with the node's attach secret, ask for
//! `elastos://peer/*` (action `message`) and wait until the node's operator
//! approves it in Home, then call the `peer` provider with that token. The
//! token is bound to this attach session, so one `Carrier` lives for the run.
//!
//! Messages travel as gossip `message` text: the sealed bytes in base64url.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

pub struct Carrier {
    api: String,
    bearer: String,
    capability: String,
    capsule: String,
    /// This party's sender id on the topic; its own messages are skipped on receive.
    pub sender_id: String,
}

fn http(method: &str, url: &str, bearer: Option<&str>, body: Option<&Value>) -> Result<Value> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
    let mut req = agent.request(method, url).set("content-type", "application/json");
    if let Some(b) = bearer {
        req = req.set("Authorization", &format!("Bearer {b}"));
    }
    let res = match body {
        Some(b) => req.send_string(&b.to_string()),
        None => req.call(),
    };
    match res {
        Ok(r) => Ok(serde_json::from_str(&r.into_string()?).unwrap_or(Value::Null)),
        Err(ureq::Error::Status(code, r)) => bail!("{method} {url}: {code} {}", r.into_string().unwrap_or_default()),
        Err(e) => Err(e.into()),
    }
}

impl Carrier {
    /// Attaches to the node in `coords` (its `home-runtime-coords.json`), asks
    /// for the peer capability and waits up to `approval` for the operator.
    pub fn connect(coords: &Path, capsule: &str, sender_id: &str, reason: &str, approval: Duration) -> Result<Self> {
        let c: Value = serde_json::from_slice(&std::fs::read(coords).with_context(|| format!("reading {}", coords.display()))?)?;
        let api = c["api_url"].as_str().context("no api_url")?.trim_end_matches('/').to_string();
        anyhow::ensure!(
            api.starts_with("http://127.0.0.1:") || api.starts_with("http://localhost:"),
            "the Runtime API must be local"
        );
        let secret = c["attach_secret"].as_str().context("no attach_secret")?;
        let attach = http("POST", &format!("{api}/api/auth/attach"), None, Some(&json!({"secret": secret, "scope": "shell"})))?;
        let bearer = attach["token"].as_str().context("attach gave no token")?.to_string();
        let ask = json!({"resource": "elastos://peer/*", "action": "message", "reason": reason});
        let mut status = http("POST", &format!("{api}/api/capability/request"), Some(&bearer), Some(&ask))?;
        let id = status["request_id"].as_str().unwrap_or_default().to_string();
        if status["status"] == "pending" {
            println!("   waiting for approval in Home: \"{reason}\" (elastos://peer/*, message), request {id}");
        }
        let start = Instant::now();
        loop {
            match status["status"].as_str() {
                Some("granted") => break,
                Some("pending") if start.elapsed() < approval => {
                    std::thread::sleep(Duration::from_secs(2));
                    status = http("GET", &format!("{api}/api/capability/request/{id}"), Some(&bearer), None)?;
                }
                Some("pending") => bail!("no approval for the peer capability within {approval:?}"),
                other => bail!("the peer capability was not granted: {} {}", other.unwrap_or("?"), status["reason"]),
            }
        }
        let capability = status["token"].as_str().context("granted without a token")?.to_string();
        Ok(Self { api, bearer, capability, capsule: capsule.into(), sender_id: sender_id.into() })
    }

    /// The same session as another party on the topic (a local run where both
    /// parties use one node: one approval, two sender ids).
    pub fn party(&self, sender_id: &str) -> Self {
        Self {
            api: self.api.clone(),
            bearer: self.bearer.clone(),
            capability: self.capability.clone(),
            capsule: self.capsule.clone(),
            sender_id: sender_id.into(),
        }
    }

    /// A `peer` provider op; returns its `data`.
    pub fn op(&self, op: &str, mut body: Value) -> Result<Value> {
        body["op"] = json!(op);
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
        let res = agent
            .post(&format!("{}/api/provider/peer/{op}", self.api))
            .set("content-type", "application/json")
            .set("Authorization", &format!("Bearer {}", self.bearer))
            .set("X-Capability-Token", &self.capability)
            .set("X-Elastos-Capsule-Id", &self.capsule)
            .send_string(&body.to_string());
        let reply: Value = match res {
            Ok(r) => serde_json::from_str(&r.into_string()?)?,
            Err(ureq::Error::Status(code, r)) => bail!("peer/{op}: {code} {}", r.into_string().unwrap_or_default()),
            Err(e) => return Err(e.into()),
        };
        if reply["status"] != "ok" {
            bail!("peer/{op}: {} {}", reply["code"], reply["message"]);
        }
        Ok(reply.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Joins `topic`; joining twice on one node is fine. With `peers` (carrier node ids), the
    /// topic is joined with exactly those nodes as its first neighbours: how a node reaches
    /// another one that does not share its discovery (two local nodes, a test).
    pub fn join_with(&self, topic: &str, peers: &[String]) -> Result<()> {
        self.join(topic)?;
        if !peers.is_empty() {
            self.op("gossip_join_peers", json!({"topic": topic, "peers": peers}))?;
        }
        Ok(())
    }

    /// Joins `topic`; joining twice on one node is fine.
    pub fn join(&self, topic: &str) -> Result<()> {
        match self.op("gossip_join", json!({"topic": topic})) {
            Err(e) if e.to_string().contains("already_joined") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    pub fn send(&self, topic: &str, bytes: &[u8]) -> Result<()> {
        self.op(
            "gossip_send",
            json!({"topic": topic, "message": URL_SAFE_NO_PAD.encode(bytes), "sender": "neptune-swap", "sender_id": self.sender_id}),
        )?;
        Ok(())
    }

    /// Everything on `topic` since this party's last receive, others' only.
    pub fn recv(&self, topic: &str) -> Result<Vec<Vec<u8>>> {
        let data = self.op(
            "gossip_recv",
            json!({"topic": topic, "consumer_id": self.sender_id, "skip_sender_id": self.sender_id, "limit": 50}),
        )?;
        Ok(data["messages"]
            .as_array()
            .map(|m| m.iter().filter_map(|m| URL_SAFE_NO_PAD.decode(m["content"].as_str()?).ok()).collect())
            .unwrap_or_default())
    }
}
