//! Browser bindings for light-core. The page fetches; this verifies.

use light_core::neptune_primitives::network::Network;
use light_core::rpc;
use light_core::tip::verify_tip;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Verify a main-net tip: `tip` is a `chain_tip` reply, `parent` a
/// `wallet_getBlocks` reply holding the tip's parent, `now_ms` the page's clock.
/// Resolves to the verified tip as JSON; rejects with the reason it failed.
#[wasm_bindgen(js_name = verifyTip)]
pub async fn verify_tip_js(tip: String, parent: String, now_ms: f64) -> Result<String, JsError> {
    let tip = rpc::parse_tip(&tip).map_err(|e| JsError::new(&format!("{e:#}")))?;
    let mut parents = rpc::parse_blocks(&parent).map_err(|e| JsError::new(&format!("{e:#}")))?;
    if parents.len() != 1 {
        return Err(JsError::new("expected exactly one parent block"));
    }
    let verified = verify_tip(tip, parents.remove(0), Network::Main, now_ms as u64)
        .await
        .map_err(|e| JsError::new(&format!("{e:#}")))?;
    Ok(serde_json::to_string(&verified)?)
}
