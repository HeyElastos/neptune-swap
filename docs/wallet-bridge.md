# How the app asks elastOS Wallet for an EVM transaction (runtime fork)

Runtime fork (branch neptune-dev, patches/elastos-runtime-v2.0.1-neptune.patch)
lets the `neptune-swap` capsule use the Browser app's EIP-1193 wallet bridge. Requests are bound to the
app that made them (`requested_by_actor` = the launch token's actor). Known gap: the bridge's audit records
still say capsule "browser".

All calls: same-origin fetch with header `x-elastos-home-token: <home_token>` (as runtime.rs does for the
provider), JSON bodies. The gateway sets `page_url`/`origin` for apps other than the Browser itself
(`https://neptune-swap.app.elastos`), whatever the app sends; account access is granted per origin, 12 h. Chain: `"eip155:8453"` (Base) or `"eip155:20"` (Elastos ESC), the only EVM chains the bridge serves; (in the dev Home this is the local anvil
chain, `scripts/dev-home.sh` sets BASE_RPC_URL).

1. Account access (once per origin): `POST /api/apps/browser/wallet/request-accounts`
   `{chain_namespace, page_url, origin}` → `{approval_request: {request_id, status}, requires_approval: true}`.
   The user approves in the Wallet app. Poll `GET /api/apps/browser/wallet/approvals/<request_id>?page_url=..&origin=..`
   until `status` is completed; the payload then carries the disclosed account (`account_id`, `address`).
2. Transaction: `POST /api/apps/browser/wallet/request-transaction`
   `{method: "eth_sendTransaction", params: [{from, to, value: "0x..", data: "0x.."}], account_id, chain_namespace, address, page_url, origin}`.
   `from` must equal `address`. Data ≤ 256 KiB. → an approval request; the user approves in Wallet with a passkey.
3. Poll `approvals/<request_id>` as above until completed.
4. `POST /api/apps/browser/wallet/broadcast-transaction {request_id}` → `{request_id, transaction_hash}`.
5. Reads (receipts, eth_call): `POST /api/apps/browser/wallet/read {method, params, chain_namespace, address?, page_url, origin}`.

Calldata: `crates/evm-htlc` (no heavy deps; builds for wasm32):
- lock ETH: `evm_htlc::lock(receiver, hash_lock, refund_after_secs)`, value = amount in wei (hex).
- claim: `evm_htlc::claim(id, secret)`; refund: `evm_htlc::refund(id)`; id = `evm_htlc::lock_id(sender, receiver, [0;20], amount, hash_lock, refund_after)`.
- the secret from a receipt: `evm_htlc::claimed_secret(&Log{address, topics, data}, contract, id, hash_lock)`.
Contract: `contracts/htlc-evm` (deploy on the local chain: see `scripts/evm-htlc-local.sh`, forge create).
Hash lock = SHA-256 of the 32-byte secret, the same as the Neptune HTLC note's `hash_lock`.
