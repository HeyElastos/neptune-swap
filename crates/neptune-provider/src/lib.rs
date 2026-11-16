//! elastOS Runtime provider for `elastos://neptune/*`.
//!
//! Speaks the Runtime's provider-stdio protocol: one JSON request per line on
//! stdin, tagged by `op`, and one `{"status": "ok" | "error", ...}` line back.
//! The provider owns the JSON-RPC address of a local neptune-core started with
//! `--listen-rpc <loopback>:<port> --no-incoming`. App capsules ask for typed
//! operations by network id and never learn that address.

#![forbid(unsafe_code)]

mod prices;
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
    WalletSendStatus {
        network: String,
        job: String,
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
    HtlcPrepare {
        network: String,
    },
    HtlcLock {
        network: String,
        amount: String,
        fee: String,
        claim_lock: String,
        refund_after_ms: u64,
        #[serde(default)]
        swap_id: Option<String>,
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
    /// Reference prices in USD (display only), when the configuration allows
    /// asking public sources.
    Prices {},
    Shutdown {},
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    networks: Vec<NetworkConfig>,
    /// Development only: also accept `https://<host>` nodes. This breaks the
    /// project's "no domains" rule (the node sees this machine's address and
    /// the blocks it fetches) and is meant for testing against main net
    /// before an own node runs here.
    #[serde(default)]
    allow_remote_nodes: bool,
    /// Ask public price sources (CoinGecko, Kraken) for USD reference prices.
    /// They see this computer's address and when it asks; the Runtime asks
    /// the user before the capsule may call `prices`.
    #[serde(default)]
    price_sources: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkConfig {
    /// For example `neptune-main` or `neptune-testnet`.
    id: String,
    /// `http://<loopback IP>:<port>` of the node's `--listen-rpc` (or, with
    /// `allow_remote_nodes`, `https://<host>`).
    rpc: String,
}

struct Network {
    id: String,
    chain: Chain,
    /// The node's JSON-RPC URL, ending in `/`.
    rpc: String,
}

/// The node of one network, over JSON-RPC.
#[derive(Clone)]
struct HttpNode(String);

impl Node for HttpNode {
    fn handle(&self) -> Box<dyn Node + Send> {
        Box::new(self.clone())
    }

    fn call(&self, body: String) -> anyhow::Result<String> {
        let reply = ureq::post(&self.0)
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
    price_sources: bool,
    prices: prices::Prices,
}

impl Default for Provider {
    fn default() -> Self {
        Self { networks: Vec::new(), wallets: Wallets::new(None), price_sources: false, prices: Default::default() }
    }
}

impl Provider {
    /// Takes optional startup config from `NEPTUNE_PROVIDER_CONFIG` (JSON) for
    /// running outside the Runtime. The Runtime sends the same shape in `init`.
    pub fn from_env() -> Self {
        let mut provider = Self {
            networks: Vec::new(),
            wallets: Wallets::new(std::env::var_os("NEPTUNE_PROVIDER_DATA").map(PathBuf::from)),
            price_sources: false,
            prices: Default::default(),
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
                self.wallet(&network, |w, n| w.wallets.create(&n.id, n.chain, &HttpNode(n.rpc.clone())))
            }
            Request::WalletImport { network, phrase, from_height } => self.wallet(&network, |w, n| {
                w.wallets.import(&n.id, n.chain, &phrase, from_height, &HttpNode(n.rpc.clone()))
            }),
            Request::WalletReceive { network } => self.wallet(&network, |w, n| w.wallets.receive(&n.id, n.chain)),
            Request::WalletSync { network } => {
                self.wallet(&network, |w, n| w.wallets.sync(&n.id, n.chain, &HttpNode(n.rpc.clone())))
            }
            Request::WalletBalance { network } => {
                self.wallet(&network, |w, n| w.wallets.balance(&n.id, n.chain, now_ms()))
            }
            Request::WalletSendStatus { network, job } => {
                self.wallet(&network, |w, n| w.wallets.send_status(&n.id, n.chain, &job))
            }
            Request::WalletAddresses { network } => self.wallet(&network, |w, n| w.wallets.addresses(&n.id, n.chain)),
            Request::WalletNewAddress { network, kind } => {
                self.wallet(&network, |w, n| w.wallets.new_address(&n.id, n.chain, &kind))
            }
            Request::WalletHistory { network } => self.wallet(&network, |w, n| w.wallets.history(&n.id, n.chain)),
            Request::WalletSend { network, to, amount, fee } => self.wallet(&network, |w, n| {
                w.wallets.send(&n.id, n.chain, &to, &amount, &fee, now_ms(), &HttpNode(n.rpc.clone()))
            }),
            Request::HtlcClaimLock { network } => self.wallet(&network, |w, n| w.wallets.htlc_claim_lock(&n.id, n.chain)),
            Request::HtlcPrepare { network } => self.wallet(&network, |w, n| w.wallets.htlc_prepare(&n.id, n.chain)),
            Request::HtlcLock { network, amount, fee, claim_lock, refund_after_ms, swap_id } => self.wallet(&network, |w, n| {
                w.wallets.htlc_lock(&n.id, n.chain, &amount, &fee, &claim_lock, refund_after_ms, swap_id.as_deref(), now_ms(), &HttpNode(n.rpc.clone()))
            }),
            Request::HtlcStatus { network, swap_id, note, from_height } => self.wallet(&network, |w, n| {
                w.wallets.htlc_status(&n.id, swap_id.as_deref(), note, from_height, &HttpNode(n.rpc.clone()))
            }),
            Request::HtlcSecret { network, swap_id } => self.wallet(&network, |w, n| w.wallets.htlc_secret(&n.id, &swap_id)),
            Request::HtlcClaim { network, note, preimage, claim_index, fee, from_height } => self.wallet(&network, |w, n| {
                w.wallets.htlc_claim(&n.id, n.chain, note, &preimage, claim_index, &fee, from_height, now_ms(), &HttpNode(n.rpc.clone()))
            }),
            Request::HtlcRefund { network, swap_id, fee } => self.wallet(&network, |w, n| {
                w.wallets.htlc_refund(&n.id, n.chain, &swap_id, &fee, now_ms(), &HttpNode(n.rpc.clone()))
            }),
            Request::Prices {} if !self.price_sources => {
                error("prices_disabled", "this Home does not ask public price sources")
            }
            Request::Prices {} => match self.prices.get(prices::fetch_json) {
                Ok(value) => ok(Some(value)),
                Err(message) => error("prices_unavailable", &message),
            },
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
            let rpc = node_url(&network.rpc, config.allow_remote_nodes)?;
            if !rpc.starts_with("http://") {
                eprintln!("neptune-provider: {} uses a remote node (development only)", network.id);
            }
            let chain = chain_of(&network.id)?;
            networks.push(Network { id: network.id, chain, rpc });
        }
        self.networks = networks;
        self.price_sources = config.price_sources;
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
        match chain_height(&found.rpc) {
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

/// The node URL for `rpc`. By default only `http://<loopback IP literal>:<port>`:
/// host names are refused, so the provider never resolves DNS or reaches a
/// remote node. With `allow_remote` (development), `https://<host>[:port]` too.
fn node_url(rpc: &str, allow_remote: bool) -> Result<String, String> {
    let refused = || format!("rpc {rpc:?} must be http://<loopback IP>:<port>");
    if let Some(host_port) = rpc.strip_prefix("http://") {
        let addr: SocketAddr = host_port.trim_end_matches('/').parse().map_err(|_| refused())?;
        return if addr.ip().is_loopback() { Ok(format!("http://{addr}/")) } else { Err(refused()) };
    }
    if let Some(host) = rpc.strip_prefix("https://").map(|h| h.trim_end_matches('/')) {
        if !allow_remote {
            return Err(refused());
        }
        let valid = !host.is_empty()
            && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
        return if valid { Ok(format!("https://{host}/")) } else { Err(refused()) };
    }
    Err(refused())
}

/// Errors carry only a fixed message: transport details name the node's
/// address, which stays in the provider's log.
fn chain_height(rpc: &str) -> Result<u64, (&'static str, String)> {
    let unreachable = |e: &dyn std::fmt::Display| {
        eprintln!("neptune-provider: node request failed: {e}");
        ("node_unreachable", "The local Neptune node did not answer.".to_string())
    };
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "chain_height", "params": [] });
    let reply = ureq::post(rpc)
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
