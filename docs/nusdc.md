# nUSDC on Neptune (draft)

**Status: a draft, test networks only.** The rule is written and tested in plain Rust
(`crates/nusdc`); the Triton VM type script that enforces it on chain is not written yet, and
nothing here has been reviewed outside this project.

## What it is

nUSDC is a coin type on Neptune Cash, next to NPT. Each nUSDC is backed 1:1 by USDC that
issuers hold in a reserve on Base (or Ethereum). Moving nUSDC is as private as moving NPT:
amounts, senders and receivers stay hidden. Users get it from market participants through
the atomic swap this project already has (USDC on Base for nUSDC on Neptune), with the
Neptune lock holding one nUSDC coin instead of one NPT coin.

## The rule (all of it)

A Neptune UTXO holds an nUSDC coin when one of its coins carries nUSDC's type script hash.
Consensus runs that type script on every transaction that spends or makes such a coin
(it collects the type script hashes of all coins and requires a proof for each). The
script enforces exactly this, and nothing else:

1. **Coins are canonical.** A coin's state is one field element below 2^63: the amount in
   units of 10^-6 USDC. A UTXO holds at most one nUSDC coin.
2. **Amounts are conserved, or a change is authorized and public.** With `in` the nUSDC the
   transaction spends and `out` the nUSDC it makes:
   - `out == in`: a transfer. Nothing else is needed, and nothing is announced.
   - `out > in`: a **mint** of `out − in`. It needs secrets for M distinct issuer keys out of
     the N fixed in the program (Neptune's own key form: a key is the Tip5 hash of a secret;
     the secrets stay in the proof's private witness), and exactly one public announcement
     `[tag, MINT, out − in]`.
   - `out < in`: a **burn** of `in − out` (redemption). It needs exactly one public
     announcement `[tag, BURN, in − out]`, and no keys.
3. **Supply is public, transfers are not.** Mints and burns are announced, so anyone can add
   up the supply and compare it with the USDC in the reserve. Transfers announce nothing.

There is no owner who can change the rule (the program is fixed by its hash), no fee, no
pause, no blacklist and no upgrade. Spending a UTXO still needs its lock script (the owner's
key); this rule governs amounts only.

## Threat model

| Attack | Why it fails |
|---|---|
| Mint without the issuers | `out > in` needs M distinct issuer secrets; a key counted twice, a wrong secret or an index out of range is refused. |
| Mint more than announced, or hide a mint | The announcement must equal `out − in` exactly, and be the only nUSDC announcement. |
| Inflate with a malformed coin | Every nUSDC coin, spent or made, must be one canonical amount below 2^63; two nUSDC coins in one UTXO are refused. |
| Overflow a sum | Amounts stay below 2^63 per coin, so sums of a transaction's coins never reach the field modulus; the reference sums in 128 bits. |
| Steal someone's nUSDC | Not this rule's job: spending needs the UTXO's lock script, as for NPT. |
| Confuse with NPT or other tokens | Only coins with nUSDC's own type script hash count; other coins ride along untouched. |
| An issuer key leaks | M-of-N: one leaked key cannot mint. Issuers rotate by issuing a new token (a new program, a new hash) and swapping holders over. |
| Reserves missing | Not preventable on chain; made visible: the announced supply against the reserve on Base. Later: a ZK proof of Ethereum consensus in the mint rule removes the issuers from the trust path. |

## Open questions for Neptune's developers

- Consensus collects every type script hash and verifies a proof for each, so custom type
  scripts look accepted at the protocol level. neptune-core's wallet software only knows
  NPT and time locks (`match_type_script_and_generate_witness`), so this project's wallet
  would build nUSDC witnesses itself. Is there a mempool or relay policy that refuses
  unknown type scripts?
- Does the NativeCurrency type script accept transactions whose UTXOs also hold other coin
  types (it appears to count only its own)?
- The best way for a type script to read the kernel's announcements (by the kernel's MAST
  path, as the HTLC lock script reads the timestamp).

## Next steps

1. The type script in tasm, modelled on NativeCurrency's structure (read and check the
   salted input and output UTXOs against the standard input, sum this coin, check the
   authorization and announcement), held to `nusdc::check` by differential tests on
   random and adversarial transactions.
2. Wallet support (hold, send, receive nUSDC), and the HTLC lock holding one nUSDC coin.
3. The reserve: an audited, existing multisig (not a new contract) on Base, so no new
   contract code is added there.
4. Independent review, then an external audit, before any real money.
