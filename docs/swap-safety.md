# Swap safety rules

How a Neptune Swap trade stays atomic, and what is still open. **Test networks only** (Neptune
regtest or testnet, a local anvil chain standing in for Base) until an independent audit; see
SECURITY.md.

A swap has two hashed timelocks: Neptune's HTLC lock script (`crates/htlc`) and the
`NeptuneHtlc` contract on Base (`contracts/htlc-evm`). Each pays its receiver with the
secret before its timeout and returns to its sender after it.

## 1. Who holds the secret

**The NPT giver holds the secret, in both directions.** Claiming a Neptune lock proves the
secret inside a STARK and publishes nothing, so the secret must first become public on the
other chain, where the NPT giver claims. If the NPT receiver held it, it could take the NPT
and leave the other side unable to claim. The protocol enforces this: the hash lock travels
only in the NPT giver's message (`check_holder` in `crates/intents-protocol`).

## 2. Order of the timeouts

The NPT giver locks first, with the **longer** timeout; the other side locks second, with the
shorter one, and the NPT giver reveals the secret by claiming it.

| Rule | Value (Base) | Where |
|---|---|---|
| Least lock on the other chain | 2 h | `Chain::min_lock_secs` |
| Gap: other chain's refund before the Neptune refund | ≥ 3 h, plus 2 h to prove and mine a Neptune claim | `gap_secs`, `NEPTUNE_SETTLE_SECS` |
| Longest Neptune lock | 48 h | `MAX_LOCK_SECS` |
| The second lock's refund time | `min(now + quoted, Neptune refund − gap)`; if less than the least lock remains, **lock nothing** | `other_refund_at` |
| Last moment to reveal | 1 h before the other chain's refund | `claim_margin_secs`, `may_reveal` |

- The Neptune refund time is read from the lock script itself, never from a message.
- Bitcoin, when added, gets longer values (6 h least lock, 6 h gap, 3 h margin): its locks
  count in blocks, and a refund can lag its target time.
- Neptune's timelock uses the transaction kernel's timestamp; nodes admit it at most about
  60 s ahead of their clock, and a proven kernel's timestamp never goes down.

## 3. One secret, exactly 32 bytes, on both sides

- The secret is exactly 32 bytes and the hash lock is SHA-256 of those 32 bytes, on both
  chains. The Neptune lock script hashes one fixed 64-byte block (32 bytes of secret and
  fixed padding, every word range-checked to u32), so a secret of another length cannot open
  it. The contract takes a `bytes32` and hashes `abi.encodePacked(preimage)`, also exactly
  32 bytes. A secret that opens one side therefore opens the other (the classic
  length-mismatch attack, where one chain accepts a longer preimage, is ruled out).
- A fresh random secret per swap (the provider's `htlc_prepare`); the contract refuses a
  second lock with the same terms (`LockExists`).

## 4. Checking the counterparty's lock before anything moves

- **Neptune lock:** the receiver checks the note against the agreed terms (lock script = the
  agreed HTLC, exactly one native-currency coin of the agreed amount, no time lock, nothing
  else), finds it on **its own** node, and waits for depth (`swap-guard`, `min_depth`).
- **Base lock:** read from the chain, never from the message, and only in the one swap
  contract: `evm_htlc::HTLC_ADDRESS`, fixed on every chain by `CREATE2` from its creation
  code. A contract elsewhere, even with identical code, is refused: its creation code could
  have pre-filled storage with fake locks. Then sender, receiver, token, amount, hash lock,
  unspent, and a refund at least the gap before the Neptune refund (`evm_leg::check_lock`).
- Tokens come from the protocol's asset table (Circle's USDC on Base), never from the
  counterparty. The contract refuses tokens that deliver less than the amount
  (fee-on-transfer), and the app approves exactly the amount, nothing more.

## 5. Chain reorganisations

- Neptune: a lock counts at a set depth on the receiver's own node (`min_depth`).
- **Open:** the Base side waits for a receipt only, not for confirmations. Before any value
  moves on a public chain: wait for a set number of blocks after the counterparty's lock
  before locking or revealing, and re-read the lock just before revealing.

## 6. Fee spikes near a timeout

- The claim margin (1 h on Base) is there so a claim confirms before a refund can race it.
- **A claim is a reveal as soon as it leaves the page**: its calldata carries the secret, so a
  claim that reverts or is not mined has still exposed it. The swap flow must keep claiming
  (with a higher fee if needed) until it lands, and never treat a failed claim as "safe to
  wait for my Neptune refund".
- **Open:** a fee-bump policy for Base claims through Wallet, and a Neptune fee that follows
  the network (fixed at 0.05 NPT in tests).

## 7. Someone claiming on the user's behalf

- Base: anyone may submit `claim` with the secret, but the coins go only to the lock's
  receiver; anyone may submit `refund`, but the coins go only to its sender. A third party
  (a watchtower) can therefore finish a swap for an offline user and gains nothing.
- Neptune: a claim needs the secret **and** the receiver's spending key; a refund needs the
  sender's key. Nobody else can spend either path.

## 8. Messages and identities

- Every message is a signed, canonical envelope from a `did:key`, size-bounded, checked
  before parsing, and sealed to its recipient when private. A fresh `did:key` per intent.
- Quotes and intents expire (at most an hour ahead); an accept must answer the exact quote
  and intent it names.

## 9. Keys

Keys never enter the capsule. The Neptune seed and spending keys stay in the provider; Base
keys stay in elastOS Wallet, which shows each transaction and asks the user to approve it.
One exception by design: the NPT giver's swap secret is handed to the app to build the Base
claim (`htlc_secret`, declared as a `rights` method).

## 10. ELA on the Elastos main chain

The main chain has no hash lock and no refund path, so ELA swaps there use a scriptless swap
(PLAN.md, "ELA main chain"): ELA is paid to a key split between the two parties, and the
Neptune lock script forces the right half public on claim or refund, with a punish path if
the NPT giver stalls. Each party proves it knows its key share before any ELA is paid
(otherwise a rogue share could make the joint key one party's alone). Not built yet; it
needs P-256 arithmetic in Triton VM and its own review.

## Before main net

An independent audit of the Neptune lock script (including its SHA-256 in tasm), the contract,
the timeout rules and the Runtime patch; the open items above closed; small amounts first.
