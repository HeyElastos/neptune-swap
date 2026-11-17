//! A solver's side of one deal with the app, in both directions, for ETH and USDC on an EVM
//! chain (Base; a local anvil chain in tests). Test networks only: the EVM key is a local
//! test key (`evm-signer`), the Neptune wallet the solver's own provider.
//!
//! - **The user gives NPT** (the solver gives ETH/USDC): quote, take the accept, check the
//!   user's NPT lock on the solver's own node (`htlc_check`), lock on the EVM chain, wait for
//!   the user's claim there (which reveals the secret), claim the NPT. Refund on the EVM
//!   chain if the user never claims.
//! - **The user gives ETH/USDC** (the solver gives NPT, so it holds the secret): quote with
//!   the hash lock, lock the NPT after the accept, check the user's EVM lock on chain, claim
//!   it (revealing the secret; the user then claims the NPT). Refund the NPT if the user
//!   never locks.

use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::SigningKey;
use evm_htlc::{decode_lock, hex, parse_address, parse_hex, parse_word, Address, LockView, Word, HTLC_ADDRESS};
use evm_signer::{Node, Wallet};
use intents_protocol::{
    asset_row, decode, may_reveal, other_refund_at, Accept, Amount, Chain, Decoded, EvmLocked, Intent, Message, Net, NptLocked, Offer, Quote,
    Signed, NEPTUNE_SETTLE_SECS, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

use crate::neptune::{npt_text, Provider};

/// NPT fee per Neptune transaction.
pub const NPT_FEE: &str = "0.05";
/// Neptune blocks the user's NPT lock must be under before the solver locks anything.
pub const NPT_MIN_DEPTH: u64 = 2;
/// EVM blocks after the user's lock before the solver reveals by claiming it.
pub const EVM_CONFIRMATIONS: u64 = 2;
/// How long a quote stands.
const QUOTE_SECS: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Quoted; waiting for the user's accept.
    Quoted,
    /// The user gives NPT: waiting for its NPT lock.
    AwaitNpt,
    /// The user gives NPT: the solver's EVM lock is out; waiting for the user's claim.
    AwaitClaim,
    /// The solver gives NPT: its NPT lock is out; waiting for the user's EVM lock.
    AwaitEvm,
    /// The solver gives NPT: the user's EVM lock checked; claim it once deep enough.
    ClaimEvm,
    Done,
    Failed,
}

pub struct Chains<'a> {
    pub neptune: &'a Provider,
    pub evm: &'a Node,
    pub wallet: &'a Wallet,
}

pub struct Deal {
    pub stage: Stage,
    pub user: String,
    intent: Decoded<Intent>,
    quote: Decoded<Quote>,
    accept: Option<Decoded<Accept>>,
    /// The solver's Neptune swap key index (its claim key when it receives NPT).
    claim_index: u64,
    /// The solver gives NPT: the provider's id for the prepared secret.
    swap_id: Option<String>,
    /// The NPT lock's note and where to search from; its refund time (ms).
    note: Option<Value>,
    from_height: u64,
    neptune_refund_ms: u64,
    /// The EVM lock's id and the block from which it is watched.
    evm_id: Option<Word>,
    evm_refund_after: u64,
    seen_at: Option<u64>,
    pub line: String,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn no0x(a: &Address) -> String {
    hex(a)[2..].to_string()
}

fn token_of(chain: Chain, symbol: &str) -> Result<Address> {
    let (_, token) = asset_row(chain, symbol).context("not a listed asset")?;
    Ok(match token {
        Some(t) => parse_address(t).context("token")?,
        None => [0; 20],
    })
}

fn sign<T: Message>(body: &T, key: &SigningKey, net: Net) -> Result<(Decoded<T>, Vec<u8>)> {
    let bytes = Signed::sign(body, key, net)?.encode()?;
    Ok((decode(&bytes, now_secs(), net)?, bytes))
}

/// The offer an intent takes, if any: the user gives what the offer wants, wants what it
/// gives, within its limits and at least at its rate. Returns what the solver delivers.
pub fn matches(offer: &Offer, intent: &Intent) -> Option<Amount> {
    if intent.give != offer.want || intent.want != offer.give {
        return None;
    }
    let deliver = offer.amount_for(intent.give_amount)?;
    (deliver >= intent.min_want_amount).then_some(deliver)
}

impl Deal {
    /// Quotes `intent` (already decoded and checked) at `deliver`. Returns the deal and the
    /// signed quote to seal to the user.
    pub fn quote(intent: Decoded<Intent>, deliver: Amount, key: &SigningKey, net: Net, c: &Chains) -> Result<(Self, Vec<u8>)> {
        let i = &intent.body;
        let other = i.other_chain();
        ensure!(other.is_evm(), "this solver trades EVM assets only");
        let solver_gives_npt = i.want.is_npt();
        let key_info = c.neptune.call("htlc_claim_lock", json!({}))?;
        let neptune_key = key_info["claim_lock"].as_str().context("swap key")?.to_string();
        let claim_index = key_info["index"].as_u64().context("swap key index")?;
        let (hash_lock, swap_id) = if solver_gives_npt {
            let p = c.neptune.call("htlc_prepare", json!({}))?;
            (Some(p["hash_lock"].as_str().context("hash lock")?.to_string()), Some(p["swap_id"].as_str().context("swap id")?.to_string()))
        } else {
            (None, None)
        };
        let evm_key = no0x(&c.wallet.address);
        let (claim_key, refund_key) = if solver_gives_npt { (evm_key, neptune_key) } else { (neptune_key, evm_key) };
        let other_lock_secs = other.min_lock_secs();
        let body = Quote {
            v: PROTOCOL_VERSION,
            intent: intent.digest.clone(),
            want_amount: deliver,
            other_lock_secs,
            neptune_lock_secs: other_lock_secs + other.gap_secs() + NEPTUNE_SETTLE_SECS,
            hash_lock,
            claim_key,
            refund_key,
            expires_at: now_secs() + QUOTE_SECS,
        };
        let (quote, bytes) = sign(&body, key, net)?;
        quote.body.answers(&intent)?;
        let user = intent.signer.to_string();
        let deal = Self {
            stage: Stage::Quoted,
            user,
            intent,
            quote,
            accept: None,
            claim_index,
            swap_id,
            note: None,
            from_height: 0,
            neptune_refund_ms: 0,
            evm_id: None,
            evm_refund_after: 0,
            seen_at: None,
            line: "quoted".into(),
        };
        Ok((deal, bytes))
    }

    pub fn intent_digest(&self) -> &str {
        &self.intent.digest
    }

    fn solver_gives_npt(&self) -> bool {
        self.intent.body.want.is_npt()
    }

    fn accept(&self) -> Result<&Decoded<Accept>> {
        self.accept.as_ref().context("no accept")
    }

    /// The user's accept. When the solver gives NPT it locks the NPT now and returns the
    /// signed `npt_locked` for the user.
    pub fn on_accept(&mut self, accept: Decoded<Accept>, key: &SigningKey, net: Net, c: &Chains) -> Result<Option<Vec<u8>>> {
        ensure!(self.stage == Stage::Quoted, "not waiting for an accept");
        accept.body.answers(&accept.signer, &self.intent, &self.quote, now_secs())?;
        self.accept = Some(accept);
        if !self.solver_gives_npt() {
            self.stage = Stage::AwaitNpt;
            self.line = "accepted; waiting for the user's NPT lock".into();
            return Ok(None);
        }
        let from_height = c.neptune.tip()?;
        let refund_ms = (now_secs() + self.quote.body.neptune_lock_secs) * 1000;
        let locked = c.neptune.call(
            "htlc_lock",
            json!({
                "amount": npt_text(self.quote.body.want_amount.units()),
                "fee": NPT_FEE,
                "claim_lock": self.accept()?.body.claim_key,
                "refund_after_ms": refund_ms,
                "swap_id": self.swap_id,
            }),
        )?;
        self.note = Some(locked["note"].clone());
        self.from_height = from_height;
        self.neptune_refund_ms = refund_ms;
        let body = NptLocked { v: PROTOCOL_VERSION, deal: self.accept()?.digest.clone(), note: locked["note"].clone(), from_height };
        let (_, bytes) = sign(&body, key, net)?;
        self.stage = Stage::AwaitEvm;
        self.line = "NPT locked; waiting for the user's EVM lock".into();
        Ok(Some(bytes))
    }

    /// The user gives NPT: its `npt_locked`. Checks the lock on the solver's own node; when
    /// it is deep enough, locks on the EVM chain and returns the signed `evm_locked`.
    pub fn on_npt_locked(&mut self, locked: Decoded<NptLocked>, key: &SigningKey, net: Net, c: &Chains) -> Result<Option<Vec<u8>>> {
        ensure!(self.stage == Stage::AwaitNpt, "not waiting for an NPT lock");
        let accept = self.accept()?.clone();
        locked.body.answers(&locked.signer, &accept, &accept.signer)?;
        self.note = Some(locked.body.note.clone());
        self.from_height = locked.body.from_height;
        self.check_npt_and_lock(key, net, c)
    }

    /// Checks the user's NPT lock again (until it is deep enough), then locks on the EVM chain.
    pub fn check_npt_and_lock(&mut self, key: &SigningKey, net: Net, c: &Chains) -> Result<Option<Vec<u8>>> {
        let accept = self.accept()?.clone();
        let least = (self.quote.body.expires_at.saturating_sub(QUOTE_SECS) + self.quote.body.neptune_lock_secs) * 1000 - 600_000;
        let check = c.neptune.call(
            "htlc_check",
            json!({
                "note": self.note.clone().context("no note")?,
                "from_height": self.from_height,
                "hash_lock": accept.body.hash_lock.clone().context("no hash lock")?,
                "claim_index": self.claim_index,
                "amount_nau": self.intent.body.give_amount.units().to_string(),
                "least_refund_after_ms": least,
                "min_depth": NPT_MIN_DEPTH,
            }),
        )?;
        if check["deep_enough"] != true {
            self.line = format!("the user's NPT lock: found {}, depth {}", check["found"], check["depth"]);
            return Ok(None);
        }
        self.neptune_refund_ms = check["refund_after_ms"].as_u64().context("refund time")?;
        let other = self.intent.body.other_chain();
        let refund_after = other_refund_at(other, self.neptune_refund_ms / 1000, self.quote.body.other_lock_secs, c.evm.now()?)?;
        let want = &self.intent.body.want;
        let token = token_of(want.chain, &want.symbol)?;
        let receiver = parse_address(&accept.body.claim_key).context("the user's EVM key")?;
        let hash_lock = parse_word(accept.body.hash_lock.as_deref().context("hash lock")?).context("hash lock")?;
        let amount = self.quote.body.want_amount.units();
        if token == [0; 20] {
            c.evm.send(c.wallet, &HTLC_ADDRESS, amount, evm_htlc::lock(&receiver, &hash_lock, refund_after))?;
        } else {
            c.evm.send(c.wallet, &token, 0, evm_htlc::approve(&HTLC_ADDRESS, amount))?;
            c.evm.send(c.wallet, &HTLC_ADDRESS, 0, evm_htlc::lock_token(&token, amount, &receiver, &hash_lock, refund_after))?;
        }
        let id = evm_htlc::lock_id(&c.wallet.address, &receiver, &token, amount, &hash_lock, refund_after);
        ensure!(lock_view(c.evm, &id)?.sender == c.wallet.address, "the EVM lock is not on chain");
        self.evm_id = Some(id);
        self.evm_refund_after = refund_after;
        let body = EvmLocked { v: PROTOCOL_VERSION, deal: accept.digest.clone(), contract: no0x(&HTLC_ADDRESS), id: hex(&id)[2..].to_string() };
        let (_, bytes) = sign(&body, key, net)?;
        self.stage = Stage::AwaitClaim;
        self.line = "EVM locked; waiting for the user's claim".into();
        Ok(Some(bytes))
    }

    /// The solver gives NPT: the user's `evm_locked`. Only the swap contract counts.
    pub fn on_evm_locked(&mut self, locked: Decoded<EvmLocked>) -> Result<()> {
        ensure!(self.stage == Stage::AwaitEvm, "not waiting for an EVM lock");
        locked.body.answers(&locked.signer, self.accept()?, &self.quote)?;
        ensure!(parse_address(&locked.body.contract) == Some(HTLC_ADDRESS), "not the swap contract");
        self.evm_id = Some(parse_word(&locked.body.id).context("lock id")?);
        self.stage = Stage::ClaimEvm;
        self.line = "the user locked on the EVM chain; checking".into();
        Ok(())
    }

    /// One pass of whatever the deal waits for on chain.
    pub fn poll(&mut self, key: &SigningKey, net: Net, c: &Chains) -> Result<Option<Vec<u8>>> {
        let now = now_secs();
        match self.stage {
            Stage::Quoted if now > self.quote.body.expires_at + 60 => {
                self.stage = Stage::Failed;
                self.line = "no accept".into();
            }
            Stage::AwaitNpt if self.note.is_some() => return self.check_npt_and_lock(key, net, c),
            Stage::AwaitNpt if now > self.quote.body.expires_at + 3 * 3600 => {
                self.stage = Stage::Failed;
                self.line = "the user never locked".into();
            }
            Stage::AwaitClaim => self.await_claim(c)?,
            Stage::AwaitEvm | Stage::ClaimEvm if now * 1000 >= self.neptune_refund_ms => {
                let swap_id = self.swap_id.clone().context("no NPT lock")?;
                c.neptune.call("htlc_refund", json!({ "swap_id": swap_id, "fee": NPT_FEE }))?;
                self.stage = Stage::Done;
                self.line = "the user never locked; NPT refunded".into();
            }
            Stage::ClaimEvm => self.claim_evm(c)?,
            _ => {}
        }
        Ok(None)
    }

    /// The user gives NPT: once its claim on the EVM chain reveals the secret, claim the NPT.
    fn await_claim(&mut self, c: &Chains) -> Result<()> {
        let id = self.evm_id.context("no EVM lock")?;
        let accept = self.accept()?.clone();
        let hash_lock = parse_word(accept.body.hash_lock.as_deref().context("hash lock")?).context("hash lock")?;
        let logs = c.evm.logs(&HTLC_ADDRESS, &evm_htlc::keccak(b"Claimed(bytes32,bytes32)"), &id)?;
        if let Some(secret) = logs.iter().find_map(|l| evm_htlc::claimed_secret(&l.as_log(), &HTLC_ADDRESS, &id, &hash_lock)) {
            c.neptune.call(
                "htlc_claim",
                json!({
                    "note": self.note.clone().context("no note")?,
                    "preimage": hex(&secret)[2..],
                    "claim_index": self.claim_index,
                    "fee": NPT_FEE,
                    "from_height": self.from_height,
                }),
            )?;
            self.stage = Stage::Done;
            self.line = "the user claimed; NPT claimed".into();
        } else if c.evm.now()? >= self.evm_refund_after {
            c.evm.send(c.wallet, &HTLC_ADDRESS, 0, evm_htlc::refund(&id))?;
            self.stage = Stage::Done;
            self.line = "the user never claimed; EVM lock refunded".into();
        }
        Ok(())
    }

    /// The solver gives NPT: check the user's EVM lock on chain, then claim it (revealing the
    /// secret) once deep enough and while there is time.
    fn claim_evm(&mut self, c: &Chains) -> Result<()> {
        let id = self.evm_id.context("no EVM lock")?;
        let view = lock_view(c.evm, &id)?;
        let accept = self.accept()?;
        let give = &self.intent.body.give;
        let hash_lock = parse_word(self.quote.body.hash_lock.as_deref().context("hash lock")?).context("hash lock")?;
        let checks = [
            (view.sender == parse_address(&accept.body.refund_key).context("user key")?, "not from the user"),
            (view.receiver == c.wallet.address, "not for this solver"),
            (view.token == token_of(give.chain, &give.symbol)?, "another token"),
            (view.amount == self.intent.body.give_amount.units(), "another amount"),
            (view.hash_lock == hash_lock, "another hash lock"),
            (!view.spent, "already spent"),
        ];
        if let Some((_, why)) = checks.iter().find(|(ok, _)| !ok) {
            self.line = format!("the user's EVM lock is wrong ({why}); waiting for the NPT refund");
            self.stage = Stage::AwaitEvm;
            return Ok(());
        }
        let latest = quantity(&c.evm.call("eth_blockNumber", json!([]))?)?;
        let first = *self.seen_at.get_or_insert(latest);
        if latest.saturating_sub(first) < EVM_CONFIRMATIONS {
            self.line = "the user's EVM lock is good; waiting for confirmations".into();
            return Ok(());
        }
        if !may_reveal(self.intent.body.other_chain(), view.refund_after, c.evm.now()?) {
            self.line = "too late to claim safely; waiting for the NPT refund".into();
            self.stage = Stage::AwaitEvm;
            return Ok(());
        }
        let secret = c.neptune.call("htlc_secret", json!({ "swap_id": self.swap_id }))?;
        let secret = parse_word(secret["secret"].as_str().context("secret")?).context("secret")?;
        c.evm.send(c.wallet, &HTLC_ADDRESS, 0, evm_htlc::claim(&id, &secret))?;
        self.stage = Stage::Done;
        self.line = "claimed on the EVM chain; the user claims the NPT next".into();
        Ok(())
    }
}

fn quantity(v: &Value) -> Result<u64> {
    u64::from_str_radix(v.as_str().context("quantity")?.trim_start_matches("0x"), 16).context("quantity")
}

pub fn lock_view(evm: &Node, id: &Word) -> Result<LockView> {
    let ret = evm.call("eth_call", json!([{ "to": hex(&HTLC_ADDRESS), "data": hex(&evm_htlc::locks(id)) }, "latest"]))?;
    let bytes = parse_hex(ret.as_str().context("eth_call")?).context("hex")?;
    match decode_lock(&bytes) {
        Some(v) => Ok(v),
        None => bail!("not a lock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use intents_protocol::Asset;

    #[test]
    fn an_intent_takes_an_offer_only_on_its_pair_limits_and_rate() {
        let npt = Asset::npt();
        let usdc = Asset { chain: Chain::Base, symbol: "USDC".into() };
        // The solver gives NPT: 1 NPT for 0.15 USDC, 1 to 1000 NPT a swap.
        let nau = 4 * 10u128.pow(30);
        let offer = Offer {
            v: PROTOCOL_VERSION,
            id: "00".repeat(16),
            give: npt.clone(),
            want: usdc.clone(),
            rate_give: Amount::new(nau),
            rate_want: Amount::new(150_000),
            min_give: Amount::new(nau),
            max_give: Amount::new(1000 * nau),
            expires_at: 2_000_000_000,
            withdrawn: false,
        };
        let intent = |give_amount: u128, min_want: u128| Intent {
            v: PROTOCOL_VERSION,
            id: "11".repeat(16),
            give: usdc.clone(),
            give_amount: Amount::new(give_amount),
            want: npt.clone(),
            min_want_amount: Amount::new(min_want),
            expires_at: 2_000_000_000,
        };
        assert_eq!(matches(&offer, &intent(15_000_000, 100 * nau)), Some(Amount::new(100 * nau)));
        assert_eq!(matches(&offer, &intent(15_000_000, 101 * nau)), None, "asks more than the rate");
        assert_eq!(matches(&offer, &intent(100_000, 1)), None, "below the offer's least");
        let swapped = Intent { give: npt.clone(), want: usdc.clone(), ..intent(15_000_000, 1) };
        assert_eq!(matches(&offer, &swapped), None, "the other direction");
    }
}
