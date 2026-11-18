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
(it collects the type script hashes of all coins, inputs and outputs, and requires a proof
for each). The script enforces exactly this, and nothing else:

1. **Coins are canonical.** A coin's state is one field element below 2^63: the amount in
   units of 10^-6 USDC. A UTXO holds at most one nUSDC coin.
2. **The total is conserved, unless M issuers take part.** With `in` the nUSDC the
   transaction spends and `out` the nUSDC it makes, summed as integers (never as field
   elements: two coins already exceed the field's modulus):
   - `out == in`: a transfer. Nothing else is needed.
   - any other change, a mint or a burn: the transaction must **spend UTXOs locked by at
     least M distinct issuer lock scripts** out of the N fixed in the program.

Issuers authorize by spending, not by sharing a secret: each issuer's own lock script is
proved against this very transaction, by that issuer alone. No secret reaches whoever builds
the proof, and an authorization cannot be reused (the issuer's UTXO is gone). M-of-N means
M independent parties.

There are no announcements: any transaction, even one with no nUSDC coin (where this script
never runs), can carry any announcement, so announcements could not prove the supply. There
is no owner who can change the rule (the program is fixed by its hash), no fee, no pause, no
blacklist and no upgrade. Spending a holder's UTXO still needs its lock script (the holder's
key); this rule governs amounts only.

## Supply and reserves

Neptune hides amounts, so the chain cannot show the total supply by itself. Issuers publish
every mint and burn (amount and transaction), signed by M of them, and keep the reserve on
Base in an audited multisig anyone can read. Redemption goes through the issuers: a holder
swaps nUSDC to them for USDC with the atomic swap, and they burn it. This is the one trust
point, made visible; a later version can mint against a STARK-verified proof of the deposit
on Ethereum (a ZK light client) and remove it.

## Threat model

| Attack | Why it fails |
|---|---|
| Mint or burn without the issuers | Any change of the total needs M distinct issuer lock scripts among the spent UTXOs; one issuer twice, an issuer only in the outputs, or fewer than M is refused. |
| Reuse an issuer's authorization | It is a spend of the issuer's UTXO in this transaction, proved by the issuer's own lock script against this kernel; the UTXO cannot be spent twice. |
| Collect issuer secrets at the prover | There are none to collect: each issuer proves its own lock script. |
| Inflate with a malformed coin | Every nUSDC coin, spent or made, must be one canonical amount below 2^63; two nUSDC coins in one UTXO are refused, even for issuers. |
| Wrap a sum in the field | Sums are integers (128-bit, u32 limbs in the script); a test shows the wrap a field sum would allow (2^32 − 3 units in against two maximal coins out). |
| Steal someone's nUSDC | Not this rule's job: spending needs the UTXO's lock script, as for NPT. |
| Confuse with NPT or other tokens | Only coins with nUSDC's own type script hash count; other coins ride along untouched. |
| An issuer key leaks | One leaked key is one of M. Issuers rotate by issuing a new token (a new program, a new hash) and swapping holders over. |
| Reserves missing | Not preventable on chain; made visible by the issuers' signed mint and burn records against the reserve on Base. |

## Open questions for Neptune's developers

- Consensus collects every type script hash and verifies a proof for each, so custom type
  scripts look accepted at the protocol level. neptune-core's wallet software only knows
  NPT and time locks (`match_type_script_and_generate_witness`), so this project's wallet
  would build nUSDC witnesses itself. Is there a mempool or relay policy that refuses
  unknown type scripts?
- Does the NativeCurrency type script accept transactions whose UTXOs also hold other coin
  types (it appears to count only its own)?
- Stock wallets ignore UTXOs holding an unknown coin type (`all_type_script_states_are_valid`),
  NPT in the same UTXO included; an nUSDC UTXO should hold nUSDC only, and this project's
  wallet handles it.
- ProofCollection-backed transactions show their type script hashes until upgraded to a
  SingleProof: nUSDC use is visible on such a transaction before the upgrade.

## Next steps

1. The type script in tasm, modelled on NativeCurrency's structure (read and check the
   salted input and output UTXOs against the standard input, sum this coin in u32 limbs,
   find M distinct issuer lock script hashes among the inputs), held to `nusdc::check` by
   differential tests on random and adversarial transactions.
2. Wallet support (hold, send, receive nUSDC), and the HTLC lock holding one nUSDC coin.
3. The reserve: an audited, existing multisig (not a new contract) on Base, so no new
   contract code is added there.
4. Independent review, then an external audit, before any real money.
