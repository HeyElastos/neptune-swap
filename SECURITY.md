# Security

Neptune Swap moves money. Please report security problems privately, never in a public issue.

## Reporting

Use GitHub's private vulnerability reporting: the **Security** tab of this repository,
**Report a vulnerability**. Say what is affected (crate, file, runtime patch), how to reproduce
it, and what an attacker gains. A proof of concept on regtest or a local chain is ideal; please
do not test against main net or against other people's funds.

You will get an answer within a few days. Fixes are published once they are ready, with credit
to the reporter unless you prefer otherwise.

## Scope

In scope: everything in this repository, including the HTLC lock script (`crates/htlc`), the
EVM contract (`contracts/htlc-evm`), the wire protocol (`crates/intents-protocol`), the wallet
and provider (`crates/light-core`, `crates/neptune-provider`), the capsule
(`crates/intents-ui`) and the elastOS runtime patch (`patches/`).

Especially welcome: anything that lets a counterparty take funds without the other side
getting theirs, breaks a timelock's ordering, forges or replays a message, leaks a key or
secret, links a user's swaps, or lets a page do more than its manifest declares.

Out of scope: bugs in neptune-core or the elastOS runtime themselves (report those upstream),
and the known limits listed in PLAN.md under "Risks" and "Known limits" and the open items
in `docs/swap-safety.md`.

## Status

This is an experimental concept, not a finished product: it has not been tested in real use or
audited. Do not use it with real funds.
