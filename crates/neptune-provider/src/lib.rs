//! elastOS Runtime provider for `elastos://neptune/*`.
//!
//! Speaks the Runtime's provider-stdio protocol: one JSON request per line on
//! stdin, tagged by `op`, and one `{"status": "ok" | "error", ...}` line back.
//! The provider owns the JSON-RPC address of a local neptune-core started with
//! `--listen-rpc <loopback>:<port> --no-incoming`. App capsules ask for typed
//! operations by network id and never learn that address.

#![forbid(unsafe_code)]

mod wallet;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use light_core::neptune_primitives::network::Network as Chain;
use light_core::swap::HtlcNote;
use wallet::{Node, Wallets};

use serde::Deserialize;
use serde_json::{json, Value};

const RUNTIME_INVOCATION_FIELD: &str = "_runtime_invocation";
const NODE_TIMEOUT: Duration = Duration::from_secs(5);
/// Block downloads are larger (up to 100 blocks per call).
const BLOCKS_TIMEOUT: Duration = Duration::from_secs(60);

// Every variant has braces: serde enforces `deny_unknown_fields` on struct
// variants of an internally tagged enum, but not on unit variants.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Init {
        #[serde(default)]
        config: Value,
    },
    Networks {},
    Status {
        network: String,
    },
    WalletStatus {
        network: String,
    },
    WalletCreate {
        network: String,
    },
    WalletImport {
        network: String,
        phrase: String,
        #[serde(default)]
        from_height: u64,
    },
    WalletReceive {
        network: String,
    },
    WalletSync {
        network: String,
    },
    WalletBalance {
        network: String,
    },
    WalletSend {
        network: String,
        to: String,
        amount: String,
        fee: String,
    },
    WalletAddresses {
        network: String,
    },
    WalletNewAddress {
        network: String,
        kind: String,
    },
    WalletHistory {
        network: String,
    },
    HtlcClaimLock {
        network: String,
    },
    HtlcLock {
        network: String,
        amount: String,
        fee: String,
        claim_lock: String,
        refund_after_ms: u64,
    },
    HtlcStatus {
        network: String,
        #[serde(default)]
        swap_id: Option<String>,
        #[serde(default)]
        note: Option<HtlcNote>,
        #[serde(default)]
        from_height: u64,
    },
    HtlcSecret {
        network: String,
        swap_id: String,
    },
    HtlcClaim {
        network: String,
        note: HtlcNote,
        preimage: String,
        claim_index: u64,
        fee: String,
        #[serde(default)]
        from_height: u64,
    },
    HtlcRefund {
        network: String,
        swap_id: String,
        fee: String,
    },
    Shutdown {},
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    networks: Vec<NetworkConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkConfig {
    /// For example `neptune-main` or `neptune-testnet`.
    id: String,
    /// `http://<loopback IP>:<port>` of the node's `--listen-rpc`.
    rpc: String,
}

struct Network {
    id: String,
    chain: Chain,
    rpc: SocketAddr,
}

/// The node of one network, over loopback JSON-RPC.
struct HttpNode(SocketAddr);

impl Node for HttpNode {
    fn call(&self, body: String) -> anyhow::Result<String> {
        let reply = ureq::post(&format!("http://{}/", self.0))
            .timeout(BLOCKS_TIMEOUT)
            .set("content-type", "application/json")
            .send_string(&body)
            .map_err(|e| {
                eprintln!("neptune-provider: node request failed: {e}");
                anyhow::anyhow!("The local Neptune node did not answer.")
            })?;
        Ok(reply.into_string()?)
    }
}

pub struct Provider {
    networks: Vec<Network>,
    wallets: Wallets,
}

impl Default for Provider {
    fn default() -> Self {
        Self { networks: Vec::new(), wallets: Wallets::new(None) }
    }
}

impl Provider {
    /// Takes optional startup config from `NEPTUNE_PROVIDER_CONFIG` (JSON) for
    /// running outside the Runtime. The Runtime sends the same shape in `init`.
    pub fn from_env() -> Self {
        let mut provider = Self {
            networks: Vec::new(),
            wallets: Wallets::new(std::env::var_os("NEPTUNE_PROVIDER_DATA").map(PathBuf::from)),
        };
        if let Ok(raw) = std::env::var("NEPTUNE_PROVIDER_CONFIG") {
            let configured = serde_json::from_str(&raw).map_err(|e| e.to_string()).and_then(|c| provider.configure(c));
            if let Err(e) = configured {
                eprintln!("neptune-provider: ignoring NEPTUNE_PROVIDER_CONFIG: {e}");
            }
        }
        provider
    }

    /// Handles one request line. Returns the response line and whether the
    /// Runtime asked the provider to stop.
    pub fn handle_line(&mut self, line: &str) -> (String, bool) {
        let (response, shutdown) = match decode(line) {
            Ok(request) => {
                let shutdown = matches!(request, Request::Shutdown {});
                (self.handle(request), shutdown)
            }
            Err(message) => (error("invalid_request", &message), false),
        };
        (response.to_string(), shutdown)
    }

    fn handle(&mut self, request: Request) -> Value {
        match request {
            // The Runtime's own bridge sends its generic ProviderConfig (base_path,
            // allowed_paths, ...): no Neptune networks in it, so the startup
            // configuration from NEPTUNE_PROVIDER_CONFIG stays.
            Request::Init { config } if config.get("networks").is_none() => ok(None),
            Request::Init { config } => match self.configure(config) {
                Ok(()) => ok(None),
                Err(message) => error("invalid_config", &message),
            },
            Request::Networks {} => {
                let ids: Vec<_> = self.networks.iter().map(|n| json!({ "id": n.id })).collect();
                ok(Some(json!({ "networks": ids })))
            }
            Request::Status { network } => self.status(&network),
            Request::WalletStatus { network } => self.wallet(&network, |w, n| w.wallets.status(&n.id, n.chain)),
            Request::WalletCreate { network } => {
                self.wallet(&network, |w, n| w.wallets.create(&n.id, n.chain, &HttpNode(n.rpc)))
            }
            Request::WalletImport { network, phrase, from_height } => self.wallet(&network, |w, n| {
                w.wallets.import(&n.id, n.chain, &phrase, from_height, &HttpNode(n.rpc))
            }),
            Request::WalletReceive { network } => self.wallet(&network, |w, n| w.wallets.receive(&n.id, n.chain)),
            Request::WalletSync { network } => {
                self.wallet(&network, |w, n| w.wallets.sync(&n.id, n.chain, &HttpNode(n.rpc)))
            }
            Request::WalletBalance { network } => {
                self.wallet(&network, |w, n| w.wallets.balance(&n.id, n.chain, now_ms()))
            }
            Request::WalletAddresses { network } => self.wallet(&network, |w, n| w.wallets.addresses(&n.id, n.chain)),
            Request::WalletNewAddress { network, kind } => {
                self.wallet(&network, |w, n| w.wallets.new_address(&n.id, n.chain, &kind))
            }
            Request::WalletHistory { network } => self.wallet(&network, |w, n| w.wallets.history(&n.id, n.chain)),
            Request::WalletSend { network, to, amount, fee } => self.wallet(&network, |w, n| {
                w.wallets.send(&n.id, n.chain, &to, &amount, &fee, now_ms(), &HttpNode(n.rpc))
            }),
            Request::HtlcClaimLock { network } => self.wallet(&network, |w, n| w.wallets.htlc_claim_lock(&n.id, n.chain)),
            Request::HtlcLock { network, amount, fee, claim_lock, refund_after_ms } => self.wallet(&network, |w, n| {
                w.wallets.htlc_lock(&n.id, n.chain, &amount, &fee, &claim_lock, refund_after_ms, now_ms(), &HttpNode(n.rpc))
            }),
            Request::HtlcStatus { network, swap_id, note, from_height } => self.wallet(&network, |w, n| {
                w.wallets.htlc_status(&n.id, swap_id.as_deref(), note, from_height, &HttpNode(n.rpc))
            }),
            Request::HtlcSecret { network, swap_id } => self.wallet(&network, |w, n| w.wallets.htlc_secret(&n.id, &swap_id)),
            Request::HtlcClaim { network, note, preimage, claim_index, fee, from_height } => self.wallet(&network, |w, n| {
                w.wallets.htlc_claim(&n.id, n.chain, note, &preimage, claim_index, &fee, from_height, now_ms(), &HttpNode(n.rpc))
            }),
            Request::HtlcRefund { network, swap_id, fee } => self.wallet(&network, |w, n| {
                w.wallets.htlc_refund(&n.id, n.chain, &swap_id, &fee, now_ms(), &HttpNode(n.rpc))
            }),
            Request::Shutdown {} => ok(None),
        }
    }

    fn configure(&mut self, config: Value) -> Result<(), String> {
        let config: Config = serde_json::from_value(config).map_err(|e| e.to_string())?;
        let mut networks = Vec::with_capacity(config.networks.len());
        for network in config.networks {
            let id_ok = network.id.starts_with("neptune-")
                && network.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !id_ok {
                return Err(format!("network id {:?} must be neptune-<a-z0-9->", network.id));
            }
            let rpc = loopback_rpc(&network.rpc)?;
            let chain = chain_of(&network.id)?;
            networks.push(Network { id: network.id, chain, rpc });
        }
        self.networks = networks;
        Ok(())
    }

    /// Runs a wallet operation on a configured network; errors become a
    /// `wallet_error` reply with the operation's message.
    fn wallet(&self, network: &str, op: impl FnOnce(&Self, &Network) -> anyhow::Result<Value>) -> Value {
        let Some(found) = self.networks.iter().find(|n| n.id == network) else {
            return error("unknown_network", &format!("{network} is not configured"));
        };
        match op(self, found) {
            Ok(data) => ok(Some(data)),
            Err(e) => error("wallet_error", &format!("{e:#}")),
        }
    }

    fn status(&self, network: &str) -> Value {
        let Some(found) = self.networks.iter().find(|n| n.id == network) else {
            return error("unknown_network", &format!("{network} is not configured"));
        };
        match chain_height(found.rpc) {
            Ok(height) => ok(Some(json!({ "network": found.id, "height": height }))),
            Err((code, message)) => error(code, &message),
        }
    }
}

/// The chain a network id names: `neptune-main`, `neptune-testnet` or
/// `neptune-regtest` (mock proofs, local development).
fn chain_of(id: &str) -> Result<Chain, String> {
    let name = id.strip_prefix("neptune-").unwrap_or(id);
    light_core::parse_network(name).map_err(|_| format!("network id {id:?} names no Neptune network (main, testnet, regtest)"))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Accepts only `http://<loopback IP literal>:<port>`. Host names are refused,
/// so the provider never resolves DNS or reaches a remote node.
fn loopback_rpc(rpc: &str) -> Result<SocketAddr, String> {
    let refused = || format!("rpc {rpc:?} must be http://<loopback IP>:<port>");
    let host_port = rpc.strip_prefix("http://").ok_or_else(refused)?.trim_end_matches('/');
    let addr: SocketAddr = host_port.parse().map_err(|_| refused())?;
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(refused())
    }
}

/// Errors carry only a fixed message: transport details name the node's
/// address, which stays in the provider's log.
fn chain_height(rpc: SocketAddr) -> Result<u64, (&'static str, String)> {
    let unreachable = |e: &dyn std::fmt::Display| {
        eprintln!("neptune-provider: node request failed: {e}");
        ("node_unreachable", "The local Neptune node did not answer.".to_string())
    };
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "chain_height", "params": [] });
    let reply = ureq::post(&format!("http://{rpc}/"))
        .timeout(NODE_TIMEOUT)
        .set("content-type", "application/json")
        .send_string(&request.to_string())
        .map_err(|e| unreachable(&e))?
        .into_string()
        .map_err(|e| unreachable(&e))?;
    let node_error = || ("node_error", "The local Neptune node sent an unexpected reply.".to_string());
    let reply: Value = serde_json::from_str(&reply).map_err(|_| node_error())?;
    if let Some(err) = reply.get("error") {
        eprintln!("neptune-provider: node error: {err}");
        return Err(node_error());
    }
    reply["result"]["height"].as_u64().ok_or_else(node_error)
}

fn decode(line: &str) -> Result<Request, String> {
    let mut value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let object = value.as_object_mut().ok_or("request must be a JSON object")?;
    // The Runtime has already checked the caller's capability for this op; the
    // read operations here need nothing else from its invocation envelope.
    object.remove(RUNTIME_INVOCATION_FIELD);
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn ok(data: Option<Value>) -> Value {
    match data {
        Some(data) => json!({ "status": "ok", "data": data }),
        None => json!({ "status": "ok" }),
    }
}

fn error(code: &str, message: &str) -> Value {
    json!({ "status": "error", "code": code, "message": message })
}

#[cfg(test)]
mod tests;
