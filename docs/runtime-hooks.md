# What the Runtime is missing for an outside app and its provider

Neptune Swap runs today on a patched elastOS Runtime v2.0.1
(`patches/elastos-runtime-v2.0.1-neptune.patch`, about 350 lines). Every change in it is a
special case for one app or one provider. This page lists them as **general features**: an
app or provider declares what it needs in its signed manifest, the user approves what needs
approving, and the Runtime enforces it. With these in place, the patch is not needed.

Both manifests (`capsules/neptune-swap/capsule.json`, `capsules/neptune-provider/capsule.json`)
already declare everything below and pass the Runtime's own `CapsuleManifest::validate`. What
is missing is the Runtime acting on those declarations.

## 1. Third-party native providers under their own scheme

*Today:* the scheme name is added to the reserved sub-provider names, the gateway allowlists
the provider's operations for one app id, and the server spawns one named binary at startup.

*General feature:*
- A signed provider capsule (`role: provider`, `provides: elastos://<scheme>/*`) installed
  in a Home is started, checksum-verified, and registered under its scheme. Nothing about
  the scheme is compiled into the Runtime.
- The provider's `authority.capabilities` lists its operations by action (read, write,
  execute, admin). The Runtime refuses any operation the provider did not declare (B3).
- An app reaches a provider operation only when the app's manifest names it (by resource and
  operation), and only through the gateway with the app's launch token. Any other
  operation, or any other app, gets 404 / 403, as the allowlist does now.
- The provider gets its configuration from the Home (a per-provider config file in the data
  directory), not from a gateway environment variable.

## 2. The Wallet bridge for apps other than the Browser

*Today:* a list of apps that may use the EIP-1193 bridge, and an app page and origin that
the gateway sets.

*General feature:*
- An app that declares Wallet methods (account access, transactions; risk `rights` and
  `payment`, approval `user`) may call the bridge's request-accounts, request-transaction,
  approval-status and broadcast routes.
- **The gateway names the app, never the app itself.** Wallet prompts show the app as
  `https://<app id>.app.elastos` (or better, the app's signed name), whatever page or origin
  the app sends. An app cannot present itself as some website in a Wallet prompt.
- Approvals stay bound to the launch token's actor, so one app cannot read or finish
  another's (already so in the bridge).
- Open item: Wallet's audit records still name the Browser for these requests; they should
  name the app.

## 3. Typed chain reads for apps that show no web pages

*Today:* bridge reads require the Browser's internet consent, which only the Browser can
hold; the patch skips it for the other bridge apps and limits them to five methods.

*General feature:*
- An app declares the chain reads it makes (method names, chains). The Runtime answers
  only those, through its chain provider, with the usual audit, and without the Browser's
  internet consent (the app gets typed answers, not network access).
- Swap apps need `eth_blockNumber`, `eth_call`, `eth_getCode`, `eth_getLogs`,
  `eth_getTransactionReceipt`, plus a block-header read (`eth_getBlockByNumber`, latest) so
  timeouts can follow the chain's clock rather than the computer's.
- The Runtime caps `eth_getLogs` block ranges, for every caller.

## 4. Carrier message topics owned by an app

*Today:* one app may use `gossip_join/leave/send/recv`, only on topics under its own
prefix, at most four joined per launch; the gateway rebuilds each request, deriving the
sender id and receive cursor from the launch and the topic.

*General feature:*
- An app declares topic patterns it owns (under its own name) and a message size limit. The
  Runtime allows the gossip operations on those topics only, with a per-launch limit on
  topics joined.
- The sender id is the Runtime's, derived from the launch and the topic, never the launch
  id itself and never chosen by the app: the app cannot speak as anyone else, and its
  messages on two topics do not link.
- Rate limits per launch and per topic (the patch has a topic limit, not a message rate).

## 5. User approval when a provider reaches the network for an app

*Today:* the price operation asks public price sources, so the gateway asks the user in Home
first (a capability request), remembers a yes for 12 hours and a no for 10 minutes, one
pending request at a time, and never asks again while a request is open.

*General feature:*
- A provider operation marked `approval: user` in the manifest goes through a Home approval
  per user before the Runtime forwards it, with the manifest's description as the prompt.
  The Runtime keeps the answer for a declared time and rate-limits re-asking.
- Providers declare their rights in the manifest and the Runtime enforces them; today the
  provider restricts itself. The neptune provider's draft is
  `capsules/neptune-provider/rights.json`: connect to the loopback node only, to two price
  hosts only after approval, listen nowhere, exec only the `neptune-prover` binary from its
  own bundle (real proving runs out of process), files only in its data folder.

## 6. Installing a signed outside app

*Today:* the development Home's `components.json` gets the app under `external` and the
provider's checksum, by a script; an installed Home refuses this because its
`components.json` is pinned by the signed release.

*General feature:* a Home installs a signed capsule bundle by its fingerprint (the capsule
kit's build and verify), without touching the release's own pinned files. This is roadmap
check P2-10's last step: Neptune Swap installed and running with no Runtime change.

## What stays in the app and provider

Keys, proving, the swap rules and every check of a counterparty stay in the provider and the
app; none of it belongs in the Runtime. The Runtime only has to give typed, declared,
approved, audited doors.
