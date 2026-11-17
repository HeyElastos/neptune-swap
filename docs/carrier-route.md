# The carrier (peer) route for the neptune-swap capsule

Runtime fork (`the elastOS runtime checkout`, branch neptune-dev), `gateway_provider_proxy.rs`:
the gateway serves `POST /api/provider/peer/<op>` to the neptune-swap capsule only, the
same way as `/api/provider/neptune/<op>` (same-origin fetch, header `x-elastos-home-token`).

Ops and bodies (anything else in the body is dropped; `get_ticket` and `gossip_join_exact` were
removed after the security review):
- `gossip_join` / `gossip_leave` `{topic}`
- `gossip_send` `{topic, message}`: message is text, 1 to 64 KiB; use the sealed bytes as base64url
- `gossip_recv` `{topic, limit ≤ 50}` → `data.messages[].content` (base64url sealed bytes)

Rules the gateway enforces:
- topic is `neptune-swap` or under `neptune-swap/` or `neptune-swap-`, a-z 0-9 `-_./`, at most 128
  chars, at most 4 joined per launch;
- sender_id and the receive cursor are the gateway's (`neptune-swap:<16 hex>` from the launch and
  the topic); a party never receives its own messages and cannot read another party's cursor;
- `gossip_join` answers `already_joined` when the topic is already joined on that node: treat it as ok.

The swap messages (intent, quote, accept, npt_locked, evm_locked) are the intents-protocol
envelopes sealed with `intents_protocol::seal` to the recipient's did:key, exactly as
`crates/solver/src/bin/swap-e2e.rs` sends them. The solver (`crates/solver/src/carrier.rs`, topic `neptune-swap-dev-<hex>` in swap-e2e) uses
sender_id "solver" on the dev Home's managed runtime (`home-runtime-coords.json`). Note: the gateway
process runs its own carrier node, so the capsule (gateway) and the solver (managed runtime) may be two
nodes; whether they find each other without tickets is still to be checked.

## Live test (from the neptune-swap page in a signed-in dev Home)

Open neptune-swap from Home (approve it if Home asks), then in that page's context:

```js
const token = new URLSearchParams(location.hash.slice(1)).get('home_token');
const peer = (op, body) => fetch(`/api/provider/peer/${op}`, {method: 'POST',
  headers: {'content-type': 'application/json', 'x-elastos-home-token': token},
  body: JSON.stringify(body)}).then(async r => [r.status, await r.text()]);
[await peer('gossip_join', {topic: 'neptune-swap-live'}),
 await peer('gossip_send', {topic: 'neptune-swap-live', message: 'aGVsbG8'}),
 await peer('gossip_recv', {topic: 'neptune-swap-live'}),
 await peer('gossip_join', {topic: 'chat'})]          // expect 400: not its topic
```

Expected: 200s with `status: ok` for the first three, the recv empty (own messages are skipped), the last one 400.
Then the solver side: a `solver::carrier::Carrier` on the managed
runtime joins `neptune-swap-live` and sends/receives, to see whether the gateway's carrier node and
the managed runtime's node exchange messages (Home asks to approve elastos://peer/* for it).
