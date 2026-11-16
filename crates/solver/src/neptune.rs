//! A party's own Neptune wallet: its `neptune-provider`, driven over stdin and
//! stdout exactly as the elastOS Runtime drives it (one JSON request line in,
//! one reply line out). Each party has its own data folder and so its own seed.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub struct Provider {
    pub binary: PathBuf,
    pub data: PathBuf,
    /// For example `neptune-regtest`.
    pub network: String,
    /// The provider's startup config (`NEPTUNE_PROVIDER_CONFIG`).
    pub config: String,
}

impl Provider {
    /// `op` with `body`'s fields, on this party's network. Returns `data`.
    pub fn call(&self, op: &str, mut body: Value) -> Result<Value> {
        body["op"] = json!(op);
        body["network"] = json!(self.network);
        let mut child = Command::new(&self.binary)
            .env("NEPTUNE_PROVIDER_CONFIG", &self.config)
            .env("NEPTUNE_PROVIDER_DATA", &self.data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", self.binary.display()))?;
        child.stdin.take().context("stdin")?.write_all(format!("{body}\n").as_bytes())?;
        let out = child.wait_with_output()?;
        let line = String::from_utf8(out.stdout)?;
        let reply: Value = serde_json::from_str(line.lines().next().context("no reply")?)?;
        if reply["status"] != "ok" {
            bail!("{op}: {}", reply["message"].as_str().unwrap_or("failed"));
        }
        Ok(reply.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Syncs until the wallet is at the node's tip.
    pub fn sync(&self) -> Result<()> {
        for _ in 0..200 {
            if self.call("wallet_sync", json!({}))?["done"] == true {
                return Ok(());
            }
        }
        bail!("the wallet does not catch up with its node")
    }

    /// Total balance in NPT, as the provider prints it.
    pub fn balance(&self) -> Result<String> {
        self.sync()?;
        Ok(self.call("wallet_balance", json!({}))?["balance"]["total"].as_str().unwrap_or("0").to_string())
    }

    pub fn tip(&self) -> Result<u64> {
        self.sync()?;
        self.call("wallet_status", json!({}))?["synced_height"].as_u64().context("no synced height")
    }
}

/// NPT smallest units (nau, 4·10^30 per NPT) as the provider's decimal text.
pub fn npt_text(units: u128) -> String {
    use light_core::neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;
    light_core::npt(NativeCurrencyAmount::from_nau(units as i128))
}
