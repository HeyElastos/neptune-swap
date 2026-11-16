//! A native EVM key: the solver's own, or a test user's on a local chain (where
//! it stands in for elastOS Wallet). Builds, signs and sends legacy EIP-155
//! transactions over JSON-RPC, the same transaction type chain-provider
//! prepares for Wallet.
//!
//! The node URL must be on this machine unless the caller says otherwise: the
//! solver talks to its own node.

#![forbid(unsafe_code)]

use anyhow::{bail, ensure, Context, Result};
use evm_htlc::{hex, keccak, parse_hex, Address, Word};
use k256::ecdsa::SigningKey;
use serde_json::{json, Value};

pub struct Wallet {
    key: SigningKey,
    pub address: Address,
}

impl Wallet {
    pub fn from_hex(secret: &str) -> Result<Self> {
        let bytes = parse_hex(secret).context("not a hex key")?;
        let key = SigningKey::from_slice(&bytes).context("not a secp256k1 key")?;
        let point = key.verifying_key().to_encoded_point(false);
        let hash = keccak(&point.as_bytes()[1..]);
        let address: Address = hash[12..].try_into().unwrap();
        Ok(Self { key, address })
    }

    /// A legacy EIP-155 transaction, signed (the raw bytes for eth_sendRawTransaction).
    pub fn sign_legacy(&self, tx: &Legacy) -> Result<Vec<u8>> {
        let unsigned = rlp_list(&[
            rlp_uint(tx.nonce.into()),
            rlp_uint(tx.gas_price),
            rlp_uint(tx.gas.into()),
            rlp_bytes(&tx.to),
            rlp_uint(tx.value),
            rlp_bytes(&tx.data),
            rlp_uint(tx.chain_id.into()),
            rlp_uint(0),
            rlp_uint(0),
        ]);
        let digest = keccak(&unsigned);
        let (signature, recovery) = self.key.sign_prehash_recoverable(&digest)?;
        let (r, s) = (signature.r().to_bytes(), signature.s().to_bytes());
        let v = u128::from(tx.chain_id) * 2 + 35 + u128::from(recovery.to_byte());
        Ok(rlp_list(&[
            rlp_uint(tx.nonce.into()),
            rlp_uint(tx.gas_price),
            rlp_uint(tx.gas.into()),
            rlp_bytes(&tx.to),
            rlp_uint(tx.value),
            rlp_bytes(&tx.data),
            rlp_uint(v),
            rlp_bytes(trim(&r)),
            rlp_bytes(trim(&s)),
        ]))
    }
}

pub struct Legacy {
    pub nonce: u64,
    pub gas_price: u128,
    pub gas: u64,
    pub to: Address,
    pub value: u128,
    pub data: Vec<u8>,
    pub chain_id: u64,
}

/// A JSON-RPC connection to an EVM node.
pub struct Node {
    url: String,
}

impl Node {
    /// A node on this machine (http://127.0.0.1 or localhost).
    pub fn local(url: &str) -> Result<Self> {
        ensure!(
            url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"),
            "the EVM node must be on this machine"
        );
        Ok(Self { url: url.to_string() })
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let text = ureq::post(&self.url)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .with_context(|| format!("{method}: the EVM node did not answer"))?
            .into_string()?;
        let reply: Value = serde_json::from_str(&text)?;
        if let Some(err) = reply.get("error") {
            bail!("{method}: {err}");
        }
        Ok(reply["result"].clone())
    }

    pub fn chain_id(&self) -> Result<u64> {
        quantity(&self.call("eth_chainId", json!([]))?).map(|n| n as u64)
    }

    pub fn balance(&self, who: &Address) -> Result<u128> {
        quantity(&self.call("eth_getBalance", json!([hex(who), "latest"]))?)
    }

    /// The latest block's timestamp (seconds): the clock HTLC refunds use.
    pub fn now(&self) -> Result<u64> {
        let block = self.call("eth_getBlockByNumber", json!(["latest", false]))?;
        quantity(&block["timestamp"]).map(|n| n as u64)
    }

    /// Signs and sends a call; returns the transaction hash after the receipt
    /// shows success.
    pub fn send(&self, wallet: &Wallet, to: &Address, value: u128, data: Vec<u8>) -> Result<Receipt> {
        let from = hex(&wallet.address);
        let nonce = quantity(&self.call("eth_getTransactionCount", json!([from, "pending"]))?)? as u64;
        let gas_price = quantity(&self.call("eth_gasPrice", json!([]))?)?;
        let estimate = json!([{ "from": from, "to": hex(to), "value": format!("0x{value:x}"), "data": hex(&data) }]);
        let gas = quantity(&self.call("eth_estimateGas", estimate)?)? as u64;
        let raw = wallet.sign_legacy(&Legacy { nonce, gas_price, gas: gas + gas / 5, to: *to, value, data, chain_id: self.chain_id()? })?;
        let hash = self.call("eth_sendRawTransaction", json!([hex(&raw)]))?;
        let hash = hash.as_str().context("no transaction hash")?.to_string();
        let receipt = self.wait_receipt(&hash)?;
        ensure!(receipt.ok, "transaction {hash} reverted");
        Ok(receipt)
    }

    pub fn wait_receipt(&self, hash: &str) -> Result<Receipt> {
        for _ in 0..120 {
            let r = self.call("eth_getTransactionReceipt", json!([hash]))?;
            if !r.is_null() {
                return Receipt::from_json(hash, &r);
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        bail!("no receipt for {hash}")
    }

    /// Logs of `contract` with first topic `topic0` and second topic `topic1`.
    pub fn logs(&self, contract: &Address, topic0: &Word, topic1: &Word) -> Result<Vec<LogEntry>> {
        let filter = json!([{ "address": hex(contract), "fromBlock": "0x0", "toBlock": "latest",
                              "topics": [hex(topic0), hex(topic1)] }]);
        let logs = self.call("eth_getLogs", filter)?;
        logs.as_array().context("logs")?.iter().map(LogEntry::from_json).collect()
    }
}

pub struct Receipt {
    pub hash: String,
    pub ok: bool,
    pub block: u64,
    pub logs: Vec<LogEntry>,
}

impl Receipt {
    fn from_json(hash: &str, r: &Value) -> Result<Self> {
        Ok(Self {
            hash: hash.to_string(),
            ok: r["status"].as_str() == Some("0x1"),
            block: quantity(&r["blockNumber"])? as u64,
            logs: r["logs"].as_array().context("logs")?.iter().map(LogEntry::from_json).collect::<Result<_>>()?,
        })
    }
}

pub struct LogEntry {
    pub address: Address,
    pub topics: Vec<Word>,
    pub data: Vec<u8>,
}

impl LogEntry {
    fn from_json(l: &Value) -> Result<Self> {
        Ok(Self {
            address: evm_htlc::parse_address(l["address"].as_str().context("address")?).context("address")?,
            topics: l["topics"]
                .as_array()
                .context("topics")?
                .iter()
                .map(|t| t.as_str().and_then(evm_htlc::parse_word).context("topic"))
                .collect::<Result<_>>()?,
            data: parse_hex(l["data"].as_str().context("data")?).context("data")?,
        })
    }

    pub fn as_log(&self) -> evm_htlc::Log<'_> {
        evm_htlc::Log { address: &self.address, topics: &self.topics, data: &self.data }
    }
}

pub fn quantity(v: &Value) -> Result<u128> {
    let s = v.as_str().context("not a quantity")?;
    u128::from_str_radix(s.trim_start_matches("0x"), 16).context("not a quantity")
}

fn trim(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

fn rlp_uint(n: u128) -> Vec<u8> {
    rlp_bytes(trim(&n.to_be_bytes()))
}

fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return b.to_vec();
    }
    [rlp_length(b.len(), 0x80), b.to_vec()].concat()
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let body = items.concat();
    [rlp_length(body.len(), 0xc0), body].concat()
}

fn rlp_length(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        vec![offset + len as u8]
    } else {
        let bytes = trim(&(len as u64).to_be_bytes()).to_vec();
        [vec![offset + 55 + bytes.len() as u8], bytes].concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // anvil's documented account 0 and its address.
    const KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn the_address_comes_from_the_key() {
        let w = Wallet::from_hex(KEY).unwrap();
        assert_eq!(hex(&w.address), "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    }

    #[test]
    fn rlp_matches_known_encodings() {
        assert_eq!(rlp_uint(0), vec![0x80]);
        assert_eq!(rlp_uint(0x7f), vec![0x7f]);
        assert_eq!(rlp_uint(0x400), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(rlp_list(&[rlp_bytes(b"cat"), rlp_bytes(b"dog")]), vec![0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']);
        assert_eq!(rlp_bytes(&[0u8; 60])[..2], [0xb8, 60]);
    }

    #[test]
    fn only_local_nodes() {
        assert!(Node::local("https://mainnet.base.org").is_err());
        assert!(Node::local("http://127.0.0.1:8545").is_ok());
    }

    /// Against a running `anvil --chain-id 8453`: a signed transfer goes through.
    #[test]
    #[ignore = "needs anvil on 127.0.0.1:8545"]
    fn a_signed_transfer_lands_on_anvil() {
        let node = Node::local("http://127.0.0.1:8545").unwrap();
        let alice = Wallet::from_hex(KEY).unwrap();
        let to = [0x42; 20];
        let before = node.balance(&to).unwrap();
        let receipt = node.send(&alice, &to, 12345, vec![]).unwrap();
        assert!(receipt.ok);
        assert_eq!(node.balance(&to).unwrap() - before, 12345);
    }
}
