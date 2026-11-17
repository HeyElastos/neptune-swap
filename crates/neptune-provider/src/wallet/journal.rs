//! The app's own record of each swap it is in, kept per network in `<network>/journal.json`
//! (owner-only), so a page that was closed mid-swap can pick it up again: which step it
//! reached, the messages and locks so far, what to claim or refund and when. The provider
//! only stores and returns it; the app reads it as its own data and checks what it acts on
//! against the chains again.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{ensure, Result};
use serde_json::{json, Map, Value};

use super::Wallets;

/// Bytes per record and records per network: room for every open swap, not for a flood.
const RECORD_MAX_BYTES: usize = 32 * 1024;
const RECORDS_MAX: usize = 200;

impl Wallets {
    fn journal(&self, network_id: &str) -> Result<Map<String, Value>> {
        match fs::read(self.dir(network_id)?.join("journal.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Keeps `record` under `id` (16 to 64 lowercase hex), replacing an earlier one.
    pub fn swap_save(&self, network_id: &str, id: &str, record: Value) -> Result<Value> {
        ensure!((16..=64).contains(&id.len()) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "a swap id is 16 to 64 lowercase hex");
        ensure!(record.is_object(), "a swap record is an object");
        ensure!(serde_json::to_vec(&record)?.len() <= RECORD_MAX_BYTES, "the swap record is too large");
        let mut journal = self.journal(network_id)?;
        if !journal.contains_key(id) && journal.len() >= RECORDS_MAX {
            // Room for a new swap: drop finished ones (never an open one, which may hold funds).
            journal.retain(|_, r| !matches!(r["step"].as_str(), Some("done" | "failed")));
        }
        ensure!(journal.contains_key(id) || journal.len() < RECORDS_MAX, "too many open swap records");
        journal.insert(id.to_string(), record);
        let dir = self.dir(network_id)?;
        fs::create_dir_all(&dir)?;
        let tmp = dir.join("journal.json.tmp");
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(&journal)?)?;
        file.sync_all()?;
        fs::rename(tmp, dir.join("journal.json"))?;
        Ok(json!({ "saved": id }))
    }

    pub fn swap_list(&self, network_id: &str) -> Result<Value> {
        Ok(json!({ "swaps": self.journal(network_id)? }))
    }
}
