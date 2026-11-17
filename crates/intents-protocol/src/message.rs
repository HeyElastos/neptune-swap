use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Decoded, DidKey, Error, MAX_LOCK_SECS, MAX_TTL_SECS, NEPTUNE_SETTLE_SECS, PROTOCOL_VERSION};

/// A message body that travels inside a [`crate::Signed`] envelope.
pub trait Message: Serialize + DeserializeOwned {
    const KIND: &'static str;

    /// Checks the message on its own. `now` is Unix seconds.
    fn validate(&self, now: u64) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chain {
    Neptune,
    Bitcoin,
    Ethereum,
    /// Base (EVM, chain id 8453), where elastOS Wallet pays.
    Base,
}

impl Chain {
    /// The EIP-155 chain id of an EVM chain; None for the UTXO chains.
    pub fn evm_chain_id(self) -> Option<u64> {
        match self {
            Chain::Ethereum => Some(1),
            Chain::Base => Some(8453),
            Chain::Neptune | Chain::Bitcoin => None,
        }
    }

    pub fn is_evm(self) -> bool {
        self.evm_chain_id().is_some()
    }

    /// Shortest HTLC the NPT giver can safely claim from: time to confirm the lock,
    /// then [`Chain::claim_margin_secs`] to spare.
    pub fn min_lock_secs(self) -> u64 {
        match self {
            Chain::Bitcoin => 6 * 3600,
            _ => 2 * 3600,
        }
    }

    /// The NPT giver claims at least this long before the HTLC here refunds, so the claim
    /// confirms before a refund can race it (fee spikes, pinning, replacement cycling,
    /// censorship).
    pub fn claim_margin_secs(self) -> u64 {
        match self {
            Chain::Bitcoin => 3 * 3600,
            _ => 3600,
        }
    }

    /// Least time between this chain's refund and the Neptune refund: room for a reveal
    /// that lands as late as the chain allows, then a Neptune claim proved and mined.
    pub fn gap_secs(self) -> u64 {
        match self {
            // Height locks: the refund can lag its target time by slow blocks, and the
            // claim branch stays open until the refund confirms.
            Chain::Bitcoin => 6 * 3600,
            _ => 3 * 3600,
        }
    }

    /// Lowercase hex length of a claim or refund key on this chain: a Tip5 digest the
    /// HTLC lock script commits to (Neptune), a compressed secp256k1 key (Bitcoin), an
    /// address (EVM, no `0x`).
    fn key_hex_len(self) -> usize {
        match self {
            Chain::Neptune => 80,
            Chain::Bitcoin => 66,
            Chain::Ethereum | Chain::Base => 40,
        }
    }
}

/// Every asset the protocol trades: chain, symbol, smallest units per coin, and the
/// token contract where the asset is one. A verifier checks a lock against this row,
/// never against a symbol alone: ETH on Ethereum and ETH on Base are different assets.
/// Every chain here locks by itself (an HTLC on the chain): no bridge is ever part of a
/// swap. USDC is Circle's own on each chain, not a bridged copy. The Elastos main chain
/// accepts only fixed signature programs (no hash lock, no refund path), so ELA joins
/// through a scriptless swap (PLAN.md, "ELA main chain"), not an HTLC there.
pub const ASSETS: [(Chain, &str, u128, Option<&str>); 6] = [
    (Chain::Neptune, "NPT", 4 * 10u128.pow(30), None),
    (Chain::Ethereum, "ETH", 10u128.pow(18), None),
    (Chain::Ethereum, "USDC", 10u128.pow(6), Some("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")),
    (Chain::Base, "ETH", 10u128.pow(18), None),
    (Chain::Base, "USDC", 10u128.pow(6), Some("833589fcd6edb6e08f4c7c32d4f71b54bda02913")),
    (Chain::Bitcoin, "BTC", 10u128.pow(8), None),
];

/// The listed row of an asset: smallest units per coin, and its token contract (None for a
/// chain's native coin).
pub fn asset_row(chain: Chain, symbol: &str) -> Option<(u128, Option<&'static str>)> {
    ASSETS.iter().find(|(c, s, ..)| *c == chain && *s == symbol).map(|(_, _, units, token)| (*units, *token))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub chain: Chain,
    pub symbol: String,
}

impl Asset {
    pub fn npt() -> Self {
        Self { chain: Chain::Neptune, symbol: "NPT".into() }
    }

    pub fn is_npt(&self) -> bool {
        *self == Self::npt()
    }

    pub(crate) fn validate(&self, field: &'static str) -> Result<(), Error> {
        if ASSETS.iter().any(|(chain, symbol, ..)| *chain == self.chain && *symbol == self.symbol) {
            Ok(())
        } else {
            Err(Error::invalid(field, "not a listed asset"))
        }
    }
}

/// An amount in the asset's smallest unit. On the wire it is a decimal string,
/// so it survives JSON readers that turn large numbers into floats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Amount(u128);

impl Amount {
    pub fn new(units: u128) -> Self {
        Self(units)
    }

    pub fn units(self) -> u128 {
        self.0
    }

    /// Reads a typed amount such as "1.25" exactly: at most `decimals` digits
    /// after the point, each typed step worth `unit` smallest units (NPT:
    /// 30 decimals, unit 4, since 1 NPT = 4 * 10^30 nau).
    pub fn parse_decimal(text: &str, decimals: u32, unit: u128) -> Result<Self, Error> {
        let text = text.trim();
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || !digits(whole) || !digits(fraction) || (text.contains('.') && fraction.is_empty()) {
            return Err(Error::invalid("amount", "must be a number like 12 or 0.5"));
        }
        if fraction.len() > decimals as usize {
            return Err(Error::invalid("amount", "has more decimals than the asset"));
        }
        let scaled = format!("{whole}{fraction:0<width$}", width = decimals as usize);
        let units = scaled
            .parse::<u128>()
            .ok()
            .and_then(|n| n.checked_mul(unit))
            .ok_or(Error::invalid("amount", "too large"))?;
        Ok(Self(units))
    }
}

impl TryFrom<String> for Amount {
    type Error = Error;

    fn try_from(s: String) -> Result<Self, Error> {
        let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
        if !canonical {
            return Err(Error::invalid("amount", "must be a decimal integer without leading zeros"));
        }
        // Neptune amounts are i128: anything above cannot be a real amount on any chain here.
        s.parse::<u128>()
            .ok()
            .filter(|&n| n <= i128::MAX as u128)
            .map(Self)
            .ok_or(Error::invalid("amount", "too large"))
    }
}

impl From<Amount> for String {
    fn from(a: Amount) -> Self {
        a.0.to_string()
    }
}

/// What a user gives and the least it wants back. One side is always NPT.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub v: u16,
    /// 16 random bytes as lowercase hex.
    pub id: String,
    pub give: Asset,
    pub give_amount: Amount,
    pub want: Asset,
    pub min_want_amount: Amount,
    /// Unix seconds after which solvers ignore the intent.
    pub expires_at: u64,
}

impl Intent {
    /// The chain of the side that is not NPT.
    pub fn other_chain(&self) -> Chain {
        if self.give.is_npt() { self.want.chain } else { self.give.chain }
    }
}

impl Message for Intent {
    const KIND: &'static str = "intent";

    fn validate(&self, now: u64) -> Result<(), Error> {
        check_version(self.v)?;
        check_hex("id", &self.id, 32)?;
        check_expiry(self.expires_at, now)?;
        self.give.validate("give")?;
        self.want.validate("want")?;
        if self.give.is_npt() == self.want.is_npt() {
            return Err(Error::invalid("give", "exactly one side must be NPT"));
        }
        check_positive("give_amount", self.give_amount)?;
        check_positive("min_want_amount", self.min_want_amount)
    }
}

/// A solver's offer for one intent, with the HTLC timeouts it will honour and the keys
/// it claims and refunds with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    pub v: u16,
    /// Digest of the intent this answers (see [`crate::decode`]).
    pub intent: String,
    /// What the solver delivers on the intent's `want` side.
    pub want_amount: Amount,
    /// How long the HTLC on the other chain stays claimable before it refunds.
    pub other_lock_secs: u64,
    /// How long the Neptune HTLC stays claimable before it refunds: always longer, since it is
    /// claimed last, with the preimage the first claim made public on the other chain.
    pub neptune_lock_secs: u64,
    /// SHA-256 of the 32-byte secret, lowercase hex: here when the solver gives NPT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_lock: Option<String>,
    /// The solver's key on the intent's `give` chain (it receives that asset).
    pub claim_key: String,
    /// The solver's key on the intent's `want` chain (it gives that asset).
    pub refund_key: String,
    pub expires_at: u64,
}

impl Quote {
    /// Checks that this quote answers `intent`: its digest, the minimum met, keys for the
    /// right chains, timeouts that fit the other chain, and the hash lock present exactly
    /// when the solver is the NPT giver. The caller also checks that the quote's signer is
    /// a solver the intent was sealed to.
    pub fn answers(&self, intent: &Decoded<Intent>) -> Result<(), Error> {
        let i = &intent.body;
        if self.intent != intent.digest {
            return Err(Error::invalid("intent", "does not match the intent"));
        }
        if self.want_amount < i.min_want_amount {
            return Err(Error::invalid("want_amount", "below the intent's minimum"));
        }
        check_key("claim_key", &self.claim_key, i.give.chain)?;
        check_key("refund_key", &self.refund_key, i.want.chain)?;
        let other = i.other_chain();
        if self.other_lock_secs < other.min_lock_secs() {
            return Err(Error::invalid("other_lock_secs", "too short for the other chain"));
        }
        let least = self.other_lock_secs.saturating_add(other.gap_secs() + NEPTUNE_SETTLE_SECS);
        if self.neptune_lock_secs < least || self.neptune_lock_secs > MAX_LOCK_SECS {
            return Err(Error::invalid("neptune_lock_secs", "outside the gap and the maximum"));
        }
        check_holder("hash_lock", &self.hash_lock, i.want.is_npt())
    }
}

impl Message for Quote {
    const KIND: &'static str = "quote";

    fn validate(&self, now: u64) -> Result<(), Error> {
        check_version(self.v)?;
        check_hex("intent", &self.intent, 64)?;
        check_expiry(self.expires_at, now)?;
        check_positive("want_amount", self.want_amount)?;
        check_hash_lock(&self.hash_lock)
    }
}

/// The user's choice of one quote, with the keys it claims and refunds with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    pub v: u16,
    /// Digest of the quote it picks, which in turn names the intent.
    pub quote: String,
    /// SHA-256 of the 32-byte secret, lowercase hex: here when the user gives NPT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_lock: Option<String>,
    /// The user's key on the intent's `want` chain.
    pub claim_key: String,
    /// The user's key on the intent's `give` chain.
    pub refund_key: String,
}

impl Accept {
    /// Checks that this accept, signed by `signer`, picks `quote` for `intent` while the
    /// quote stands: same signer as the intent, the quote's digest, keys for the right
    /// chains, and the hash lock present exactly when the user is the NPT giver.
    pub fn answers(&self, signer: &DidKey, intent: &Decoded<Intent>, quote: &Decoded<Quote>, now: u64) -> Result<(), Error> {
        if *signer != intent.signer {
            return Err(Error::invalid("signer", "not the intent's signer"));
        }
        if self.quote != quote.digest {
            return Err(Error::invalid("quote", "does not match the quote"));
        }
        quote.body.answers(intent)?;
        check_expiry(quote.body.expires_at, now)?;
        check_key("claim_key", &self.claim_key, intent.body.want.chain)?;
        check_key("refund_key", &self.refund_key, intent.body.give.chain)?;
        check_holder("hash_lock", &self.hash_lock, intent.body.give.is_npt())
    }
}

impl Message for Accept {
    const KIND: &'static str = "accept";

    fn validate(&self, _now: u64) -> Result<(), Error> {
        check_version(self.v)?;
        check_hex("quote", &self.quote, 64)?;
        check_hash_lock(&self.hash_lock)
    }
}

/// The NPT giver's lock, sent to the NPT receiver only: the Neptune HTLC note (the lock's
/// terms and the coin's opening data), as the wallet produced it. The receiver checks the note
/// against the agreed terms and on its own node before it locks anything.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NptLocked {
    pub v: u16,
    /// Digest of the Accept that fixed the deal.
    pub deal: String,
    /// The HTLC note (a JSON object; light-core's `HtlcNote`).
    pub note: serde_json::Value,
    /// The sender's node tip before the lock: the receiver searches from here.
    pub from_height: u64,
}

impl NptLocked {
    /// Checks that this lock belongs to `accept` and comes from the NPT giver.
    pub fn answers(&self, signer: &DidKey, accept: &Decoded<Accept>, npt_giver: &DidKey) -> Result<(), Error> {
        if self.deal != accept.digest {
            return Err(Error::invalid("deal", "not this deal's accept"));
        }
        if signer != npt_giver {
            return Err(Error::invalid("signer", "not the NPT giver"));
        }
        Ok(())
    }
}

impl Message for NptLocked {
    const KIND: &'static str = "npt_locked";

    fn validate(&self, _now: u64) -> Result<(), Error> {
        check_version(self.v)?;
        check_hex("deal", &self.deal, 64)?;
        if !self.note.is_object() {
            return Err(Error::invalid("note", "not an object"));
        }
        Ok(())
    }
}

/// The NPT receiver's lock on the other (EVM) chain, sent to the NPT giver: where it is. The
/// giver reads the lock's terms from the chain itself, never from this message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvmLocked {
    pub v: u16,
    /// Digest of the Accept that fixed the deal.
    pub deal: String,
    /// The HTLC contract, 40 lowercase hex without `0x`.
    pub contract: String,
    /// The lock's id in that contract, 64 lowercase hex.
    pub id: String,
}

impl EvmLocked {
    /// Checks that this lock belongs to `accept` and comes from the quoting solver.
    pub fn answers(&self, signer: &DidKey, accept: &Decoded<Accept>, quote: &Decoded<Quote>) -> Result<(), Error> {
        if self.deal != accept.digest {
            return Err(Error::invalid("deal", "not this deal's accept"));
        }
        if *signer != quote.signer {
            return Err(Error::invalid("signer", "not the solver that quoted"));
        }
        Ok(())
    }
}

impl Message for EvmLocked {
    const KIND: &'static str = "evm_locked";

    fn validate(&self, _now: u64) -> Result<(), Error> {
        check_version(self.v)?;
        check_hex("deal", &self.deal, 64)?;
        check_hex("contract", &self.contract, 40)?;
        check_hex("id", &self.id, 64)
    }
}

/// When the HTLC on `chain` must refund, given the Neptune HTLC's refund time as read from
/// its lock script (never from a message) and the quoted duration. Errs when less than the
/// chain's minimum lock remains: then the NPT receiver does not lock.
pub fn other_refund_at(chain: Chain, neptune_refund_at: u64, other_lock_secs: u64, now: u64) -> Result<u64, Error> {
    let at = now
        .saturating_add(other_lock_secs)
        .min(neptune_refund_at.saturating_sub(chain.gap_secs()));
    if at >= now.saturating_add(chain.min_lock_secs()) {
        Ok(at)
    } else {
        Err(Error::invalid("neptune_refund_at", "too close: lock nothing"))
    }
}

/// Whether the NPT giver may still reveal the secret by claiming the HTLC on `chain` that
/// refunds at `other_refund_at`. Past this point it waits for its own Neptune refund.
pub fn may_reveal(chain: Chain, other_refund_at: u64, now: u64) -> bool {
    now.saturating_add(chain.claim_margin_secs()) <= other_refund_at
}

fn check_hash_lock(hash_lock: &Option<String>) -> Result<(), Error> {
    hash_lock.as_deref().map_or(Ok(()), |h| check_hex("hash_lock", h, 64))
}

/// The hash lock comes from the NPT giver and from no one else.
fn check_holder(field: &'static str, hash_lock: &Option<String>, sender_gives_npt: bool) -> Result<(), Error> {
    match (hash_lock.is_some(), sender_gives_npt) {
        (true, true) | (false, false) => Ok(()),
        (false, true) => Err(Error::invalid(field, "missing: the NPT giver sets it")),
        (true, false) => Err(Error::invalid(field, "only the NPT giver sets it")),
    }
}

fn check_key(field: &'static str, key: &str, chain: Chain) -> Result<(), Error> {
    check_hex(field, key, chain.key_hex_len())
}

pub(crate) fn check_version(v: u16) -> Result<(), Error> {
    if v == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(Error::Version(v))
    }
}

pub(crate) fn check_hex(field: &'static str, s: &str, len: usize) -> Result<(), Error> {
    if s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        Err(Error::invalid(field, "wrong length or not lowercase hex"))
    }
}

/// Live, and not further ahead than [`MAX_TTL_SECS`].
pub(crate) fn check_expiry(expires_at: u64, now: u64) -> Result<(), Error> {
    if expires_at <= now {
        Err(Error::Expired(expires_at))
    } else if expires_at > now.saturating_add(MAX_TTL_SECS) {
        Err(Error::invalid("expires_at", "too far ahead"))
    } else {
        Ok(())
    }
}

pub(crate) fn check_positive(field: &'static str, amount: Amount) -> Result<(), Error> {
    if amount.units() > 0 {
        Ok(())
    } else {
        Err(Error::invalid(field, "must be positive"))
    }
}
