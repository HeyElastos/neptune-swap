//! One swap as the taker runs it, as a state machine with no I/O: the driver
//! (`swap_ui.rs`) carries out what the current step needs (a provider call, a Wallet
//! transaction, a chain read, a message on the carrier) and feeds the answer back. Every
//! answer is checked here before the swap moves on, so the rules live in one place and are
//! tested without chains.
//!
//! Two directions, one rule: the NPT giver holds the secret (docs/swap-safety.md).
//! - **The user gives NPT** (wants ETH or USDC on Base): accept with the hash lock, lock the
//!   NPT, wait for the solver's lock on Base, check it on the chain, claim it (which reveals
//!   the secret; the solver then claims the NPT). If the solver never locks, refund the NPT.
//! - **The user gets NPT** (gives ETH or USDC on Base): accept with this wallet's claim key,
//!   check the solver's NPT lock on this Home's own node, lock on Base, read the secret from
//!   the solver's claim, claim the NPT. If the solver never claims, refund on Base.
//!
//! The whole swap is kept in the provider's journal after every step (it serializes), so a
//! closed page picks it up again. Messages are kept as their checked bodies, digests and
//! signers, never re-decoded from bytes (they would have expired).

use ed25519_dalek::SigningKey;
use evm_htlc::{hex, parse_address, parse_word, Address, LockView, Word};
use intents_protocol::{
    decode, may_reveal, other_refund_at, seal, Accept, Chain, Decoded, DidKey, EvmLocked, Intent, Message, Net, NptLocked, Quote, Signed,
    PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::evm_leg::{self, Expected, Tx};

/// NPT fee per Neptune transaction of a swap.
pub const NPT_FEE: &str = "0.05";
/// Neptune blocks the solver's NPT lock must be under before the user locks anything.
pub const NPT_MIN_DEPTH: u64 = 6;
/// Base blocks under the solver's lock (counted from the lock's own block) before the user
/// reveals the secret by claiming it, and under the user's claim before the swap counts as
/// done: about 40 seconds on Base, well past its re-org depth in practice.
pub const EVM_CONFIRMATIONS: u64 = 20;
/// Neptune blocks under a claim or refund before it counts as done.
pub const NPT_SETTLED_DEPTH: u64 = 3;
/// How long after the quote the user waits for the solver's NPT lock before giving up
/// (nothing of the user's is locked yet).
const NPT_LOCK_WAIT_SECS: u64 = 3 * 3600;
/// Slack for the solver's clock when it sets its NPT refund time.
const CLOCK_SLACK_SECS: u64 = 600;

/// A checked message, kept as what was checked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Kept<T> {
    pub body: T,
    pub signer: String,
    pub digest: String,
}

impl<T: Clone> Kept<T> {
    fn of(d: &Decoded<T>) -> Self {
        Self { body: d.body.clone(), signer: d.signer.to_string(), digest: d.digest.clone() }
    }

    fn decoded(&self) -> Result<Decoded<T>, String> {
        let signer = DidKey::parse(&self.signer).map_err(|e| e.to_string())?;
        Ok(Decoded { body: self.body.clone(), signer, digest: self.digest.clone() })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The intent went to the solver; waiting for its quote.
    Asked,
    /// The quote checked out; the accept is next (it needs keys from the provider).
    Quoted,
    /// Gives NPT: the accept went out; lock the NPT next.
    Accepted,
    /// Gives NPT: the NPT lock is being proven.
    LockingNpt,
    /// Gives NPT: waiting for the solver's lock on Base.
    AwaitEvmLock,
    /// Gives NPT: the solver named its lock; reading and checking it on the chain.
    CheckEvmLock,
    /// Gives NPT: claiming on Base, which reveals the secret. Keep claiming until it lands.
    Claiming,
    /// Gets NPT: the accept went out; waiting for the solver's NPT lock.
    AwaitNptLock,
    /// Gets NPT: checking the solver's NPT lock on this Home's own node.
    CheckNptLock,
    /// Gets NPT: locking on Base through Wallet.
    LockingEvm,
    /// Gets NPT: waiting for the solver's claim on Base, which reveals the secret.
    AwaitSecret,
    /// Gets NPT: claiming the NPT with the secret.
    ClaimingNpt,
    /// Gives NPT: the solver did not lock in time; taking the NPT back.
    RefundingNpt,
    /// Gets NPT: the solver did not claim in time; taking the Base lock back.
    RefundingEvm,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    /// The intent's id (32 hex).
    pub id: String,
    pub main_net: bool,
    pub gives_npt: bool,
    pub step: Step,
    /// What happened last, for the screen.
    pub line: String,
    /// The one-time `did:key` of this intent: its 32-byte seed, hex. A messaging key only;
    /// it never controls funds.
    key: String,
    pub solver: String,
    intent: Kept<Intent>,
    quote: Option<Kept<Quote>>,
    accept: Option<Kept<Accept>>,
    /// The user's Base account (40 hex, no 0x).
    account: Option<String>,
    /// Gives NPT: the provider's id for the prepared secret and the lock.
    swap_id: Option<String>,
    /// Gets NPT: the index of this wallet's claim key.
    claim_index: Option<u64>,
    /// The NPT lock's note, where to search for it, and when it refunds (ms).
    note: Option<Value>,
    from_height: Option<u64>,
    neptune_refund_ms: Option<u64>,
    /// The Base lock: its id, refund time, the block to search for the claim from.
    evm_id: Option<String>,
    evm_refund_after: Option<u64>,
    evm_from_block: Option<u64>,
    /// The provider job proving a Neptune transaction of this swap.
    pub job: Option<String>,
    accepted_at: u64,
    /// Gives NPT: the claim on Base has gone to Wallet, so the secret counts as revealed.
    #[serde(default)]
    pub claim_sent: bool,
    /// Gets NPT: the NPT claim (or, giving NPT, the refund) was submitted; it counts once
    /// the lock is seen spent on chain.
    #[serde(default)]
    pub submitted_at: Option<u64>,
    /// The last message sent, sealed and base64url, and when: sent again while the other side
    /// has not answered (the carrier gives no delivery receipt).
    #[serde(default)]
    pub last_sent: Option<(String, u64)>,
    /// Gets NPT: the Base lock went to Wallet (it may land later even if the page lost track).
    #[serde(default)]
    pub lock_broadcast: bool,
    /// Gives NPT: the Base block the solver's lock was made in, once found.
    #[serde(default)]
    pub evm_lock_block: Option<u64>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn did(key: &SigningKey) -> DidKey {
    DidKey::new(key.verifying_key())
}

fn hex_seed(seed: &[u8; 32]) -> String {
    seed.iter().map(|b| format!("{b:02x}")).collect()
}

fn no0x(a: &Address) -> String {
    hex(a)[2..].to_string()
}

impl Swap {
    /// A new swap: `intent` sealed to `solver` under a fresh one-time DID made from `seed`
    /// (32 random bytes). Returns the swap and the sealed intent to send.
    pub fn start(intent: Intent, solver: &str, main_net: bool, seed: [u8; 32], now: u64) -> Result<(Self, Vec<u8>), String> {
        let solver_did = DidKey::parse(solver).map_err(|e| format!("The solver's DID: {e}"))?;
        let key = SigningKey::from_bytes(&seed);
        let net = if main_net { Net::Main } else { Net::Test };
        let bytes = Signed::sign(&intent, &key, net).and_then(|s| s.encode()).map_err(|e| e.to_string())?;
        let decoded: Decoded<Intent> = decode(&bytes, now, net).map_err(|e| e.to_string())?;
        let sealed = seal(&solver_did, &bytes).map_err(|e| e.to_string())?;
        let swap = Self {
            id: intent.id.clone(),
            main_net,
            gives_npt: intent.give.is_npt(),
            step: Step::Asked,
            line: "Sent to the solver; waiting for its quote.".into(),
            key: hex_seed(&seed),
            solver: solver.to_string(),
            intent: Kept::of(&decoded),
            quote: None,
            accept: None,
            account: None,
            swap_id: None,
            claim_index: None,
            note: None,
            from_height: None,
            neptune_refund_ms: None,
            evm_id: None,
            evm_refund_after: None,
            evm_from_block: None,
            job: None,
            accepted_at: 0,
            claim_sent: false,
            submitted_at: None,
            last_sent: None,
            lock_broadcast: false,
            evm_lock_block: None,
        };
        Ok((swap, sealed))
    }

    fn net(&self) -> Net {
        if self.main_net { Net::Main } else { Net::Test }
    }

    fn signing_key(&self) -> SigningKey {
        let seed: [u8; 32] = parse_word(&self.key).unwrap_or([0; 32]);
        SigningKey::from_bytes(&seed)
    }

    /// This swap's one-time DID: messages sealed to it are this swap's.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn me(&self) -> String {
        did(&self.signing_key()).to_string()
    }

    /// The Base account the swap was accepted with (40 hex, no 0x).
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    /// The NPT lock's note and the height to search for it from.
    pub fn npt_note(&self) -> Option<&Value> {
        self.note.as_ref()
    }

    pub fn npt_from_height(&self) -> u64 {
        self.from_height.unwrap_or(0)
    }

    pub fn swap_id(&self) -> Option<&str> {
        self.swap_id.as_deref()
    }

    pub fn intent(&self) -> &Intent {
        &self.intent.body
    }

    /// The other side's chain (Base).
    fn other(&self) -> Chain {
        self.intent.body.other_chain()
    }

    /// Opens a message from the deals topic if it is sealed to this swap; None otherwise.
    pub fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        intents_protocol::open(&self.signing_key(), sealed).ok()
    }

    fn sign_and_seal<T: Message>(&self, body: &T, to: &str, now: u64) -> Result<(Decoded<T>, Vec<u8>), String> {
        let bytes = Signed::sign(body, &self.signing_key(), self.net()).and_then(|s| s.encode()).map_err(|e| e.to_string())?;
        let decoded = decode(&bytes, now, self.net()).map_err(|e| e.to_string())?;
        let to = DidKey::parse(to).map_err(|e| e.to_string())?;
        Ok((decoded, seal(&to, &bytes).map_err(|e| e.to_string())?))
    }

    fn quote(&self) -> Result<Decoded<Quote>, String> {
        self.quote.as_ref().ok_or("no quote")?.decoded()
    }

    fn accept_msg(&self) -> Result<Decoded<Accept>, String> {
        self.accept.as_ref().ok_or("no accept")?.decoded()
    }

    fn fail(&mut self, why: impl Into<String>) {
        self.step = Step::Failed;
        self.line = why.into();
    }

    // --- quote and accept -----------------------------------------------------------

    /// A quote from the solver this intent was sealed to, answering it.
    pub fn on_quote(&mut self, bytes: &[u8], now: u64) -> Result<(), String> {
        if self.step != Step::Asked {
            return Err("not waiting for a quote".into());
        }
        let quote: Decoded<Quote> = decode(bytes, now, self.net()).map_err(|e| e.to_string())?;
        if quote.signer.to_string() != self.solver {
            return Err("a quote from someone else than the solver asked".into());
        }
        quote.body.answers(&self.intent.decoded()?).map_err(|e| e.to_string())?;
        self.quote = Some(Kept::of(&quote));
        self.step = Step::Quoted;
        self.line = "Quote received and checked.".into();
        Ok(())
    }

    /// What the solver delivers on the user's `want` side, in smallest units.
    pub fn quoted_amount(&self) -> Option<u128> {
        Some(self.quote.as_ref()?.body.want_amount.units())
    }

    /// The accept, with this wallet's keys: `neptune_key` is a swap key from the provider
    /// (`htlc_claim_lock`, at `claim_index`); a user who gives NPT also brings the hash lock
    /// and its `swap_id` (`htlc_prepare`). `account` is the user's Base account.
    pub fn accept(&mut self, neptune_key: &str, claim_index: u64, prepared: Option<(&str, &str)>, account: &Address, now: u64) -> Result<Vec<u8>, String> {
        if self.step != Step::Quoted {
            return Err("no quote to accept".into());
        }
        let quote = self.quote()?;
        let (hash_lock, claim_key, refund_key) = if self.gives_npt {
            let (hash_lock, _) = prepared.ok_or("the NPT giver brings the hash lock")?;
            (Some(hash_lock.to_string()), no0x(account), neptune_key.to_string())
        } else {
            (None, neptune_key.to_string(), no0x(account))
        };
        let body = Accept { v: PROTOCOL_VERSION, quote: quote.digest.clone(), hash_lock, claim_key, refund_key };
        let (accept, sealed) = self.sign_and_seal(&body, &self.solver.clone(), now)?;
        accept.body.answers(&accept.signer, &self.intent.decoded()?, &quote, now).map_err(|e| e.to_string())?;
        self.accept = Some(Kept::of(&accept));
        self.account = Some(no0x(account));
        self.accepted_at = now;
        if self.gives_npt {
            self.swap_id = prepared.map(|(_, id)| id.to_string());
            self.step = Step::Accepted;
            self.line = "Accepted. Locking your NPT next.".into();
        } else {
            self.claim_index = Some(claim_index);
            self.step = Step::AwaitNptLock;
            self.line = "Accepted. Waiting for the solver to lock its NPT.".into();
        }
        Ok(sealed)
    }

    // --- the user gives NPT -----------------------------------------------------------

    /// The provider request that locks the NPT (`htlc_lock`, proven in the background).
    pub fn npt_lock_request(&self, npt_text: &str, now: u64) -> Result<Value, String> {
        let quote = self.quote()?;
        Ok(json!({
            "amount": npt_text,
            "fee": NPT_FEE,
            "claim_lock": quote.body.claim_key,
            "refund_after_ms": (now + quote.body.neptune_lock_secs) * 1000,
            "swap_id": self.swap_id,
            "background": true,
        }))
    }

    /// The provider's answer to the lock: keeps the note and returns the sealed `npt_locked`
    /// for the solver (sent once the proof is done).
    pub fn npt_lock_started(&mut self, reply: &Value) -> Result<(), String> {
        let note = reply.get("note").filter(|n| n.is_object()).ok_or("the provider gave no lock note")?;
        self.neptune_refund_ms = Some(note["refund_after_ms"].as_u64().ok_or("no refund time in the note")?);
        self.note = Some(note.clone());
        self.from_height = reply["from_height"].as_u64();
        self.job = reply["job"].as_str().map(str::to_string);
        self.step = Step::LockingNpt;
        self.line = "Proving your NPT lock (this takes a few minutes)…".into();
        Ok(())
    }

    /// The NPT lock was sent: the `npt_locked` message for the solver.
    pub fn npt_lock_sent(&mut self, now: u64) -> Result<Vec<u8>, String> {
        let body = NptLocked {
            v: PROTOCOL_VERSION,
            deal: self.accept_msg()?.digest,
            note: self.note.clone().ok_or("no lock note")?,
            from_height: self.from_height.unwrap_or(0),
        };
        let (_, sealed) = self.sign_and_seal(&body, &self.solver.clone(), now)?;
        self.job = None;
        self.step = Step::AwaitEvmLock;
        self.line = "Your NPT is locked. Waiting for the solver's lock on Base.".into();
        Ok(sealed)
    }

    /// The solver says where its Base lock is. Only the swap contract counts.
    pub fn on_evm_locked(&mut self, bytes: &[u8], now: u64) -> Result<(), String> {
        if !matches!(self.step, Step::AwaitEvmLock | Step::CheckEvmLock) {
            return Err("not waiting for a Base lock".into());
        }
        let locked: Decoded<EvmLocked> = decode(bytes, now, self.net()).map_err(|e| e.to_string())?;
        locked.body.answers(&locked.signer, &self.accept_msg()?, &self.quote()?).map_err(|e| e.to_string())?;
        evm_leg::check_named_contract(&locked.body.contract)?;
        self.evm_id = Some(locked.body.id);
        self.step = Step::CheckEvmLock;
        self.line = "The solver locked on Base; checking it on the chain.".into();
        Ok(())
    }

    pub fn evm_id(&self) -> Option<Word> {
        parse_word(self.evm_id.as_deref()?)
    }

    /// What the solver's Base lock must be.
    fn expected_evm_lock(&self) -> Result<Expected, String> {
        let (quote, accept) = (self.quote()?, self.accept_msg()?);
        let want = &self.intent.body.want;
        Ok(Expected {
            sender: parse_address(&quote.body.refund_key).ok_or("the solver's Base key")?,
            receiver: parse_address(&accept.body.claim_key).ok_or("your Base account")?,
            token: evm_leg::token_of(want.chain, &want.symbol)?.unwrap_or([0; 20]),
            amount: quote.body.want_amount.units(),
            hash_lock: parse_word(accept.body.hash_lock.as_deref().ok_or("no hash lock")?).ok_or("hash lock")?,
        })
    }

    /// The solver's lock as read from the chain, `confirmations` blocks deep. Ok(true) when it
    /// is safe to reveal the secret by claiming it; Ok(false) to read again later; Err when it
    /// is not the agreed lock (then wait for the NPT refund; never claim).
    pub fn check_evm_lock(&mut self, view: &LockView, confirmations: u64, now: u64) -> Result<bool, String> {
        let neptune_refund = self.neptune_refund_ms.ok_or("no NPT lock")? / 1000;
        if let Err(why) = evm_leg::check_lock(view, &self.expected_evm_lock()?, self.other(), neptune_refund, now) {
            // Not claimed, ever, unless a corrected lock comes; the NPT comes back at its timeout.
            self.line = format!("{why} Not claiming; your NPT comes back after its timeout.");
            self.step = Step::CheckEvmLock;
            return Err(why);
        }
        self.evm_refund_after = Some(view.refund_after);
        if confirmations < EVM_CONFIRMATIONS {
            self.line = format!("The solver's lock is good; waiting for confirmations ({confirmations}/{EVM_CONFIRMATIONS}).");
            return Ok(false);
        }
        self.step = Step::Claiming;
        self.line = "Claiming on Base: approve it in the Wallet app.".into();
        Ok(true)
    }

    /// Right before the claim goes to Wallet: the lock as read now must still be the agreed
    /// one with time to claim safely. Then `claim_sent` is set (save the swap before sending):
    /// from here the secret counts as revealed and the claim is retried, never re-checked.
    pub fn ready_to_reveal(&mut self, view: &LockView, now: u64) -> Result<(), String> {
        if self.claim_sent {
            return Ok(());
        }
        let neptune_refund = self.neptune_refund_ms.ok_or("no NPT lock")? / 1000;
        if let Err(why) = evm_leg::check_lock(view, &self.expected_evm_lock()?, self.other(), neptune_refund, now) {
            self.step = Step::CheckEvmLock;
            self.line = format!("{why} Not claiming; your NPT comes back after its timeout.");
            return Err(why);
        }
        self.claim_sent = true;
        Ok(())
    }

    /// The claim on Base with the secret (from the provider's `htlc_secret`). From the moment
    /// this goes to Wallet the secret counts as revealed: keep claiming until it lands.
    pub fn claim_tx(&self, secret: &Word) -> Result<Tx, String> {
        let id = self.evm_id().ok_or("no Base lock")?;
        if evm_htlc::hash_lock(secret) != self.expected_evm_lock()?.hash_lock {
            return Err("the provider's secret does not open this lock".into());
        }
        Ok(evm_leg::claim_tx(&id, secret, &self.want_label()))
    }

    // --- the user gets NPT ------------------------------------------------------------

    /// The solver's `npt_locked`: returns the provider request (`htlc_check`) that checks the
    /// lock against the agreed terms on this Home's own node.
    pub fn on_npt_locked(&mut self, bytes: &[u8], now: u64) -> Result<Value, String> {
        if self.step != Step::AwaitNptLock {
            return Err("not waiting for an NPT lock".into());
        }
        let (quote, accept) = (self.quote()?, self.accept_msg()?);
        let locked: Decoded<NptLocked> = decode(bytes, now, self.net()).map_err(|e| e.to_string())?;
        locked.body.answers(&locked.signer, &accept, &quote.signer).map_err(|e| e.to_string())?;
        self.note = Some(locked.body.note.clone());
        self.from_height = Some(locked.body.from_height);
        self.step = Step::CheckNptLock;
        self.line = "The solver locked its NPT; checking it on your node.".into();
        self.npt_check_request()
    }

    /// The `htlc_check` request for the solver's NPT lock.
    pub fn npt_check_request(&self) -> Result<Value, String> {
        let quote = self.quote()?;
        let least = (self.accepted_at.saturating_sub(CLOCK_SLACK_SECS) + quote.body.neptune_lock_secs) * 1000;
        Ok(json!({
            "note": self.note.clone().ok_or("no lock note")?,
            "from_height": self.from_height.unwrap_or(0),
            "hash_lock": quote.body.hash_lock.clone().ok_or("no hash lock")?,
            "claim_index": self.claim_index.ok_or("no claim key")?,
            "amount_nau": quote.body.want_amount.units().to_string(),
            "least_refund_after_ms": least,
            "min_depth": NPT_MIN_DEPTH,
        }))
    }

    /// The provider's answer to `htlc_check`. Ok(true) when the lock is good and deep enough
    /// (lock on Base next); Ok(false) to check again later.
    pub fn npt_checked(&mut self, reply: &Value, now: u64) -> Result<bool, String> {
        if reply["found"] != true {
            self.line = "The solver's NPT lock is not on your node yet.".into();
            return Ok(false);
        }
        if reply["deep_enough"] != true {
            self.line = format!("The solver's NPT lock is good; waiting for blocks ({}/{NPT_MIN_DEPTH}).", reply["depth"].as_u64().unwrap_or(0));
            return Ok(false);
        }
        let refund_ms = reply["refund_after_ms"].as_u64().ok_or("no refund time")?;
        let quote = self.quote()?;
        let refund_after = other_refund_at(self.other(), refund_ms / 1000, quote.body.other_lock_secs, now)
            .map_err(|_| "Too little time is left before the solver's NPT refund: not locking.".to_string())?;
        self.neptune_refund_ms = Some(refund_ms);
        self.evm_refund_after = Some(refund_after);
        self.step = Step::LockingEvm;
        self.line = "The solver's NPT is locked for you. Locking on Base: approve it in the Wallet app.".into();
        Ok(true)
    }

    /// The Base lock: an exact token approval first for USDC, then the lock.
    pub fn evm_lock_txs(&self) -> Result<Vec<Tx>, String> {
        let (quote, accept) = (self.quote()?, self.accept_msg()?);
        let give = &self.intent.body.give;
        let receiver = parse_address(&quote.body.claim_key).ok_or("the solver's Base key")?;
        let hash_lock = parse_word(quote.body.hash_lock.as_deref().ok_or("no hash lock")?).ok_or("hash lock")?;
        let _ = accept;
        Ok(evm_leg::lock_txs(
            evm_leg::token_of(give.chain, &give.symbol)?,
            self.intent.body.give_amount.units(),
            &receiver,
            &hash_lock,
            self.evm_refund_after.ok_or("no refund time")?,
            &self.give_label(),
        ))
    }

    /// The id the user's Base lock gets (the contract derives it from the terms), known
    /// before it is sent: a page reloaded mid-lock finds out whether it already landed.
    pub fn planned_evm_id(&self) -> Result<Word, String> {
        let quote = self.quote()?;
        let give = &self.intent.body.give;
        let me = parse_address(self.account.as_deref().ok_or("no account")?).ok_or("account")?;
        let receiver = parse_address(&quote.body.claim_key).ok_or("the solver's Base key")?;
        let hash_lock = parse_word(quote.body.hash_lock.as_deref().ok_or("no hash lock")?).ok_or("hash lock")?;
        let token = evm_leg::token_of(give.chain, &give.symbol)?.unwrap_or([0; 20]);
        Ok(evm_htlc::lock_id(&me, &receiver, &token, self.intent.body.give_amount.units(), &hash_lock, self.evm_refund_after.ok_or("no refund time")?))
    }

    /// The Base lock landed in `block`: the `evm_locked` message for the solver.
    pub fn evm_lock_sent(&mut self, block: u64, now: u64) -> Result<Vec<u8>, String> {
        let id = self.planned_evm_id()?;
        let body = EvmLocked { v: PROTOCOL_VERSION, deal: self.accept_msg()?.digest, contract: no0x(&evm_htlc::HTLC_ADDRESS), id: hex(&id)[2..].to_string() };
        let (_, sealed) = self.sign_and_seal(&body, &self.solver.clone(), now)?;
        self.evm_id = Some(hex(&id)[2..].to_string());
        self.evm_from_block = Some(self.evm_from_block.map_or(block, |b| b.min(block)));
        self.step = Step::AwaitSecret;
        self.line = "Locked on Base. Waiting for the solver to claim it, which reveals the secret.".into();
        Ok(sealed)
    }

    /// The Base block to search for this swap's events from: kept before anything is sent, so
    /// a reload never searches from too late.
    pub fn watch_from(&mut self, block: u64) {
        if self.evm_from_block.is_none() {
            self.evm_from_block = Some(block);
        }
    }

    /// Whether the planned Base lock still leaves the chain's least lock time before its refund.
    pub fn evm_lock_in_time(&self, now: u64) -> bool {
        self.evm_refund_after.is_some_and(|at| now.saturating_add(self.other().min_lock_secs()) <= at)
    }

    /// A sealed message just sent, kept to send again while there is no answer.
    pub fn sent(&mut self, sealed: &[u8], now: u64) {
        use base64::Engine as _;
        self.last_sent = Some((base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed), now));
    }

    /// The last message again when it went out more than `every` seconds ago and the swap still
    /// waits for the other side's answer to it.
    pub fn resend_due(&mut self, now: u64, every: u64) -> Option<Vec<u8>> {
        use base64::Engine as _;
        let waiting = matches!(self.step, Step::Asked | Step::AwaitEvmLock | Step::AwaitNptLock | Step::AwaitSecret);
        let (text, at) = self.last_sent.clone()?;
        if !waiting || now < at + every {
            return None;
        }
        self.last_sent = Some((text.clone(), now));
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).ok()
    }

    /// Proving the NPT lock failed: it may or may not have reached the chain. Nothing is sent
    /// to the solver; at the lock's timeout the refund runs (and ends quietly if the lock
    /// never landed).
    pub fn npt_lock_failed(&mut self, why: &str) {
        self.job = None;
        self.step = Step::AwaitEvmLock;
        self.line = format!("The NPT lock did not go through ({why}). If it reached the chain, it comes back after its timeout.");
    }

    pub fn neptune_refund_ms(&self) -> Option<u64> {
        self.neptune_refund_ms
    }

    pub fn evm_refund_after(&self) -> Option<u64> {
        self.evm_refund_after
    }

    pub fn evm_from_block(&self) -> u64 {
        self.evm_from_block.unwrap_or(0)
    }

    pub fn hash_lock(&self) -> Option<Word> {
        let quote = self.quote.as_ref()?;
        let accept = self.accept.as_ref()?;
        parse_word(quote.body.hash_lock.as_deref().or(accept.body.hash_lock.as_deref())?)
    }

    /// The secret, read from the solver's claim: the provider request that claims the NPT.
    pub fn npt_claim_request(&mut self, secret: &Word) -> Result<Value, String> {
        if Some(evm_htlc::hash_lock(secret)) != self.hash_lock() {
            return Err("that secret does not open this swap".into());
        }
        self.step = Step::ClaimingNpt;
        self.line = "The solver claimed on Base. Claiming your NPT (proving takes a few minutes)…".into();
        Ok(json!({
            "note": self.note.clone().ok_or("no lock note")?,
            "preimage": hex(secret)[2..],
            "claim_index": self.claim_index.ok_or("no claim key")?,
            "fee": NPT_FEE,
            "from_height": self.from_height.unwrap_or(0),
            "background": true,
        }))
    }

    // --- refunds, endings, time ---------------------------------------------------------

    /// The provider request that takes the NPT back (`htlc_refund`).
    pub fn npt_refund_request(&self) -> Result<Value, String> {
        Ok(json!({ "swap_id": self.swap_id.clone().ok_or("no NPT lock of yours")?, "fee": NPT_FEE, "background": true }))
    }

    pub fn evm_refund_tx(&self) -> Result<Tx, String> {
        Ok(evm_leg::refund_tx(&self.evm_id().ok_or("no Base lock")?, &self.give_label()))
    }

    pub fn finish(&mut self, line: impl Into<String>) {
        self.step = Step::Done;
        self.job = None;
        self.line = line.into();
    }

    /// Moves on when time has run out for the current step. Returns whether it changed.
    /// Moves on when the computer's clock (`now`) says time has run out for the current step:
    /// only for deadlines that are the app's own (quote expiry) or Neptune's (whose lock
    /// script reads transaction timestamps, which nodes keep close to real time). Base
    /// deadlines follow Base's clock, in [`Swap::tick_chain`].
    pub fn tick(&mut self, now: u64) -> bool {
        let before = self.step;
        let quote_expiry = self.quote.as_ref().map(|q| q.body.expires_at);
        match self.step {
            Step::Asked if now > self.intent.body.expires_at => self.fail("No quote came before the request expired. Nothing was locked."),
            Step::Quoted if quote_expiry.is_some_and(|at| now > at) => self.fail("The quote expired before it was accepted. Nothing was locked."),
            Step::Accepted if quote_expiry.is_some_and(|at| now > at) => self.fail("The quote expired before your NPT was locked. Nothing was locked."),
            Step::AwaitEvmLock | Step::CheckEvmLock | Step::LockingNpt if self.neptune_refund_ms.is_some_and(|ms| now * 1000 >= ms) => {
                self.step = Step::RefundingNpt;
                self.line = "The solver did not lock in time. Taking your NPT back.".into();
            }
            Step::AwaitNptLock | Step::CheckNptLock if now > self.accepted_at + NPT_LOCK_WAIT_SECS => {
                self.fail("The solver did not lock its NPT in time. Nothing of yours was locked.")
            }
            _ => {}
        }
        self.step != before
    }

    /// Moves on when a Base deadline has passed. "Has the refund opened?" follows Base's own
    /// clock (`chain_now`, the contract's `chainTime()`, the clock the contract enforces);
    /// "is it too late to reveal?" takes the later of that and the computer's clock (`now`),
    /// since the latest block's time lags on a quiet chain.
    pub fn tick_chain(&mut self, chain_now: u64, now: u64) -> bool {
        let before = self.step;
        let late = chain_now.max(now);
        match self.step {
            // Not sent yet, and too late to reveal safely: back to waiting for the NPT refund.
            Step::Claiming if !self.claim_sent && self.evm_refund_after.is_some_and(|at| !may_reveal(self.other(), at, late)) => {
                self.step = Step::CheckEvmLock;
                self.line = "Too late to claim on Base safely. Your NPT comes back after its timeout.".into();
            }
            Step::AwaitSecret if self.evm_refund_after.is_some_and(|at| chain_now >= at) => {
                self.step = Step::RefundingEvm;
                self.line = "The solver did not claim in time. Taking your Base lock back: approve it in the Wallet app.".into();
            }
            _ => {}
        }
        self.step != before
    }

    /// Whether this swap still has something to do or wait for.
    pub fn open_ended(&self) -> bool {
        !matches!(self.step, Step::Done | Step::Failed)
    }

    fn label(asset: &intents_protocol::Asset, units: u128) -> String {
        match crate::intent::row_of(asset).map(|i| &crate::intent::ASSETS[i]) {
            Some(d) => format!("{} {}", crate::offers_ui::to_text(units, d), asset.symbol),
            None => asset.symbol.clone(),
        }
    }

    pub fn give_label(&self) -> String {
        Self::label(&self.intent.body.give, self.intent.body.give_amount.units())
    }

    pub fn want_label(&self) -> String {
        let units = self.quoted_amount().unwrap_or(self.intent.body.min_want_amount.units());
        Self::label(&self.intent.body.want, units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use intents_protocol::{Amount, Asset};

    const NOW: u64 = 1_800_000_000;
    const H: u64 = 3600;
    const USER: Address = [0x05; 20];
    const SOLVER_EVM: Address = [0x50; 20];
    const NEPTUNE_KEY: &str = "11111111111111111111111111111111111111111111111111111111111111111111111111111111";
    const SOLVER_NEPTUNE_KEY: &str = "22222222222222222222222222222222222222222222222222222222222222222222222222222222";

    fn solver() -> SigningKey {
        SigningKey::from_bytes(&[9; 32])
    }

    fn intent(gives_npt: bool) -> Intent {
        let npt = Asset::npt();
        let usdc = Asset { chain: Chain::Base, symbol: "USDC".into() };
        let (give, give_amount, want, min_want_amount) = if gives_npt {
            (npt, Amount::new(4 * 10u128.pow(31)), usdc, Amount::new(150_000_000))
        } else {
            (usdc, Amount::new(250_000_000), npt, Amount::new(4 * 10u128.pow(32)))
        };
        Intent { v: PROTOCOL_VERSION, id: "00112233445566778899aabbccddeeff".into(), give, give_amount, want, min_want_amount, expires_at: NOW + 600 }
    }

    /// The solver's side: opens what the user sealed to it, and signs what it answers.
    fn solver_open<T: Message>(sealed: &[u8]) -> Decoded<T> {
        decode(&intents_protocol::open(&solver(), sealed).unwrap(), NOW, Net::Test).unwrap()
    }

    fn solver_says<T: Message>(body: &T, to: &Swap) -> Vec<u8> {
        let bytes = Signed::sign(body, &solver(), Net::Test).unwrap().encode().unwrap();
        let sealed = seal(&DidKey::parse(&to.me()).unwrap(), &bytes).unwrap();
        to.open(&sealed).expect("sealed to the swap's DID")
    }

    fn started(gives_npt: bool) -> Swap {
        let (swap, sealed) = Swap::start(intent(gives_npt), &did(&solver()).to_string(), false, [7; 32], NOW).unwrap();
        let got: Decoded<Intent> = solver_open(&sealed);
        assert_eq!(got.body, intent(gives_npt));
        assert_eq!(got.signer.to_string(), swap.me());
        swap
    }

    fn quote_for(swap: &Swap, hash_lock: Option<String>) -> Quote {
        let digest = swap.intent.digest.clone();
        let gives_npt = swap.gives_npt;
        Quote {
            v: PROTOCOL_VERSION,
            intent: digest,
            want_amount: if gives_npt { Amount::new(160_000_000) } else { Amount::new(4 * 10u128.pow(32)) },
            other_lock_secs: 2 * H,
            neptune_lock_secs: 7 * H,
            hash_lock,
            claim_key: if gives_npt { SOLVER_NEPTUNE_KEY.into() } else { no0x(&SOLVER_EVM) },
            refund_key: if gives_npt { no0x(&SOLVER_EVM) } else { SOLVER_NEPTUNE_KEY.into() },
            expires_at: NOW + 300,
        }
    }

    #[test]
    fn giving_npt_runs_from_quote_to_claim_and_survives_a_reload() {
        let mut swap = started(true);
        let secret = [3u8; 32];
        let hash_lock = hex(&evm_htlc::hash_lock(&secret))[2..].to_string();
        // Someone else's quote is refused; the solver's is taken.
        let impostor = Signed::sign(&quote_for(&swap, None), &SigningKey::from_bytes(&[8; 32]), Net::Test).unwrap().encode().unwrap();
        assert!(swap.on_quote(&impostor, NOW).unwrap_err().contains("someone else"));
        let quote = solver_says(&quote_for(&swap, None), &swap);
        swap.on_quote(&quote, NOW).unwrap();
        let sealed = swap.accept(NEPTUNE_KEY, 0, Some((&hash_lock, "aabbccddeeff0011")), &USER, NOW).unwrap();
        let accept: Decoded<Accept> = solver_open(&sealed);
        assert_eq!(accept.body.hash_lock.as_deref(), Some(hash_lock.as_str()));
        assert_eq!(accept.body.claim_key, no0x(&USER));
        assert_eq!(swap.step, Step::Accepted);

        let request = swap.npt_lock_request("10", NOW).unwrap();
        assert_eq!(request["claim_lock"], SOLVER_NEPTUNE_KEY);
        assert_eq!(request["refund_after_ms"], (NOW + 7 * H) * 1000);
        let note = json!({ "refund_after_ms": (NOW + 7 * H) * 1000, "hash_lock": hash_lock });
        swap.npt_lock_started(&json!({ "note": note, "from_height": 100, "job": "j1" })).unwrap();
        assert_eq!((swap.step, swap.job.as_deref()), (Step::LockingNpt, Some("j1")));
        let notice: Decoded<NptLocked> = solver_open(&swap.npt_lock_sent(NOW).unwrap());
        assert_eq!(notice.body.deal, accept.digest);
        assert_eq!(notice.body.from_height, 100);

        // A page reload: the journal gives the swap back as it was.
        let mut swap: Swap = serde_json::from_value(serde_json::to_value(&swap).unwrap()).unwrap();
        assert_eq!(swap.step, Step::AwaitEvmLock);

        // A lock in another contract is refused, the swap contract's is checked on chain.
        let elsewhere = EvmLocked { v: PROTOCOL_VERSION, deal: accept.digest.clone(), contract: "cc".repeat(20), id: "ab".repeat(32) };
        assert!(swap.on_evm_locked(&solver_says(&elsewhere, &swap), NOW).unwrap_err().contains("another contract"));
        let here = EvmLocked { contract: no0x(&evm_htlc::HTLC_ADDRESS), ..elsewhere };
        swap.on_evm_locked(&solver_says(&here, &swap), NOW).unwrap();
        let mut view = LockView {
            sender: SOLVER_EVM,
            receiver: USER,
            token: evm_leg::token_of(Chain::Base, "USDC").unwrap().unwrap(),
            amount: 160_000_000,
            hash_lock: evm_htlc::hash_lock(&secret),
            refund_after: NOW + 2 * H,
            spent: false,
        };
        assert_eq!(swap.check_evm_lock(&view, 2, NOW), Ok(false), "too few confirmations");
        assert_eq!(swap.step, Step::CheckEvmLock);
        view.amount -= 1;
        assert!(swap.check_evm_lock(&view, 50, NOW).unwrap_err().contains("amount"));
        assert_eq!(swap.step, Step::CheckEvmLock, "a bad lock is never claimed");
        view.amount += 1;
        assert_eq!(swap.check_evm_lock(&view, EVM_CONFIRMATIONS, NOW), Ok(true));
        assert_eq!(swap.step, Step::Claiming);
        // Right before revealing, the lock is read again: spent or late means no claim.
        let spent = LockView { spent: true, ..view.clone() };
        assert!(swap.ready_to_reveal(&spent, NOW).is_err());
        assert_eq!((swap.step, swap.claim_sent), (Step::CheckEvmLock, false));
        swap.step = Step::Claiming;
        assert!(swap.ready_to_reveal(&view, NOW + 2 * H).is_err(), "no time left to claim safely");
        swap.step = Step::Claiming;
        swap.ready_to_reveal(&view, NOW).unwrap();
        assert!(swap.claim_sent);
        assert!(swap.ready_to_reveal(&spent, NOW + 3 * H).is_ok(), "once sent, keep claiming");
        assert!(swap.claim_tx(&[4; 32]).unwrap_err().contains("does not open"));
        let claim = swap.claim_tx(&secret).unwrap();
        assert_eq!(claim.to, evm_htlc::HTLC_ADDRESS);
        assert_eq!(claim.data, evm_htlc::claim(&swap.evm_id().unwrap(), &secret));
    }

    #[test]
    fn getting_npt_checks_the_solver_lock_then_locks_on_base_and_claims_with_the_secret() {
        let mut swap = started(false);
        let secret = [5u8; 32];
        let hash_lock = hex(&evm_htlc::hash_lock(&secret))[2..].to_string();
        // The NPT giver (the solver) brings the hash lock; without it the quote is refused.
        assert!(swap.on_quote(&solver_says(&quote_for(&swap, None), &swap), NOW).is_err());
        swap.on_quote(&solver_says(&quote_for(&swap, Some(hash_lock.clone())), &swap), NOW).unwrap();
        let accept: Decoded<Accept> = solver_open(&swap.accept(NEPTUNE_KEY, 4, None, &USER, NOW).unwrap());
        assert_eq!((accept.body.claim_key.as_str(), accept.body.hash_lock.as_ref()), (NEPTUNE_KEY, None));
        assert_eq!(swap.step, Step::AwaitNptLock);

        let notice = NptLocked { v: PROTOCOL_VERSION, deal: accept.digest.clone(), note: json!({ "any": "note" }), from_height: 50 };
        let check = swap.on_npt_locked(&solver_says(&notice, &swap), NOW + 60).unwrap();
        assert_eq!(check["claim_index"], 4);
        assert_eq!(check["hash_lock"], hash_lock.as_str());
        assert_eq!(check["amount_nau"], (4 * 10u128.pow(32)).to_string());
        assert_eq!(check["least_refund_after_ms"], (NOW - CLOCK_SLACK_SECS + 7 * H) * 1000);
        assert_eq!(swap.npt_checked(&json!({ "found": true, "deep_enough": false, "depth": 2 }), NOW + 60), Ok(false));
        let refund_ms = (NOW + 7 * H) * 1000;
        assert_eq!(swap.npt_checked(&json!({ "found": true, "deep_enough": true, "refund_after_ms": refund_ms }), NOW + 3600), Ok(true));
        assert_eq!(swap.step, Step::LockingEvm);

        let txs = swap.evm_lock_txs().unwrap();
        assert_eq!(txs.len(), 2, "USDC: an exact approval, then the lock");
        assert_eq!(txs[0].data, evm_htlc::approve(&evm_htlc::HTLC_ADDRESS, 250_000_000));
        let notice: Decoded<EvmLocked> = solver_open(&swap.evm_lock_sent(1234, NOW + 3700).unwrap());
        assert_eq!(notice.body.contract, no0x(&evm_htlc::HTLC_ADDRESS));
        assert_eq!(swap.evm_from_block(), 1234);
        assert_eq!(hex(&swap.planned_evm_id().unwrap())[2..], notice.body.id);
        assert_eq!(swap.step, Step::AwaitSecret);

        assert!(swap.npt_claim_request(&[6; 32]).is_err());
        let claim = swap.npt_claim_request(&secret).unwrap();
        assert_eq!(claim["claim_index"], 4);
        assert_eq!(claim["preimage"], hex(&secret)[2..]);
        assert_eq!(swap.step, Step::ClaimingNpt);
    }

    #[test]
    fn timeouts_end_or_refund_and_never_leave_funds_behind() {
        let mut asked = started(true);
        assert!(!asked.tick(NOW + 600));
        assert!(asked.tick(NOW + 601));
        assert_eq!(asked.step, Step::Failed);

        let mut waiting = started(true);
        waiting.step = Step::AwaitEvmLock;
        waiting.neptune_refund_ms = Some((NOW + 10) * 1000);
        assert!(!waiting.tick(NOW + 9));
        assert!(waiting.tick(NOW + 10));
        assert_eq!(waiting.step, Step::RefundingNpt);

        let mut locked = started(false);
        locked.step = Step::AwaitSecret;
        locked.evm_refund_after = Some(NOW + 100);
        assert!(!locked.tick(NOW + 10_000), "the computer's clock never decides a Base deadline");
        assert!(!locked.tick_chain(NOW + 99, 0));
        assert!(locked.tick_chain(NOW + 100, 0));
        assert_eq!(locked.step, Step::RefundingEvm);

        // While claiming, time never turns the swap into a refund: the secret may be out.
        let mut claiming = started(true);
        claiming.step = Step::Claiming;
        claiming.claim_sent = true;
        claiming.neptune_refund_ms = Some(NOW * 1000);
        claiming.evm_refund_after = Some(NOW + H);
        assert!(!claiming.tick(NOW + 10 * H));
        assert!(!claiming.tick_chain(NOW + 10 * H, NOW + 10 * H));
        // Not sent yet and too late to reveal: back to waiting, then the NPT refund.
        let mut late = started(true);
        late.step = Step::Claiming;
        late.evm_refund_after = Some(NOW + H);
        late.neptune_refund_ms = Some((NOW + 5 * H) * 1000);
        assert!(!late.tick(NOW + 1));
        assert!(late.tick_chain(NOW - 10 * H, NOW + 1), "a lagging chain clock never makes a late reveal look early");
        assert_eq!(late.step, Step::CheckEvmLock);
        assert!(late.tick(NOW + 5 * H));
        assert_eq!(late.step, Step::RefundingNpt);
    }

    #[test]
    fn messages_are_sent_again_while_unanswered_and_late_locks_are_not_made() {
        let mut swap = started(false);
        swap.sent(b"sealed", NOW);
        assert_eq!(swap.resend_due(NOW + 60, 120), None);
        assert_eq!(swap.resend_due(NOW + 120, 120), Some(b"sealed".to_vec()));
        assert_eq!(swap.resend_due(NOW + 121, 120), None, "the clock restarts");
        swap.step = Step::Done;
        assert_eq!(swap.resend_due(NOW + 1000, 120), None);
        swap.evm_refund_after = Some(NOW + 2 * H);
        assert!(swap.evm_lock_in_time(NOW));
        assert!(!swap.evm_lock_in_time(NOW + 1));
        swap.watch_from(500);
        swap.watch_from(900);
        assert_eq!(swap.evm_from_block(), 500, "the first watch point stays");
    }

    #[test]
    fn messages_for_someone_else_are_not_opened() {
        let swap = started(true);
        let other = started(false);
        let bytes = Signed::sign(&quote_for(&swap, None), &solver(), Net::Test).unwrap().encode().unwrap();
        let sealed = seal(&DidKey::parse(&swap.me()).unwrap(), &bytes).unwrap();
        assert!(swap.open(&sealed).is_some());
        let other_swap = Swap { key: hex_seed(&[1; 32]), ..other };
        assert!(other_swap.open(&sealed).is_none());
    }
}
