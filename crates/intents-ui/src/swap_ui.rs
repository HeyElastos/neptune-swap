//! Runs swaps from the page: carries out what each step of the engine (`swap.rs`) needs
//! (provider calls, Wallet transactions, chain reads, sealed messages on the deals topic),
//! feeds the answers back, keeps every swap in the provider's journal after every change, and
//! shows the newest in the swap card. Every open swap in the journal is driven when the page
//! opens, not only the newest: any of them may hold funds.
//!
//! Two rules from the security review shape the steps below: nothing that may hold the other
//! side's money is given up without first looking for the secret on Base, and a Neptune claim
//! or refund counts only once the lock is seen spent on chain.

use std::cell::RefCell;
use std::rc::Rc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use intents_protocol::{NPT_NEVER_LANDED_MS, NPT_SETTLED_DEPTH};
use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::evm_leg::{self, Account};
use crate::swap::{Step, Swap};
use crate::{intent, offers_ui, runtime, wallet_ui, AddressRow, AppWindow, StepRow};

/// How often swaps look for news while they wait.
const POLL_MS: u32 = 5_000;
/// Longer pause after an error, before trying again.
const RETRY_MS: u32 = 30_000;
/// A message without an answer goes out again after this long.
const RESEND_SECS: u64 = 120;
/// A submitted Neptune claim or refund not seen on chain after this long is submitted again.
const RESUBMIT_SECS: u64 = 20 * 60;

struct Ctx {
    token: Option<String>,
    account: Rc<RefCell<Option<Account>>>,
    /// Every swap this page knows of; the last is the one the card shows.
    swaps: Vec<Swap>,
    running: bool,
    /// Why the journal could not be read or written, for the card.
    trouble: Option<String>,
    /// Finished swaps kept in the journal, for the history in Activity.
    history: Vec<Swap>,
}

type Shared = Rc<RefCell<Ctx>>;

pub fn wire(app: &AppWindow, token: Option<String>, account: Rc<RefCell<Option<Account>>>) {
    let state: Shared = Rc::new(RefCell::new(Ctx { token, account, swaps: vec![], running: false, trouble: None, history: vec![] }));
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_send_swap(move || crate::spawn(send(weak.clone(), state.clone())));
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_swap_dismiss(move || {
            let mut s = state.borrow_mut();
            if s.swaps.last().is_some_and(|w| !w.open_ended()) {
                if let Some(done) = s.swaps.pop() {
                    s.history.push(done);
                }
            }
            drop(s);
            render(&weak.unwrap(), &state);
        });
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_swap_resume(move || crate::spawn(resume(weak.clone(), state.clone())));
    }
}

/// Whether the reviewed swap can be sent, and if not, why: for the review sheet.
pub fn send_state(app: &AppWindow, account: Option<&Account>, token: bool) -> (bool, String) {
    if !token {
        return (false, "Open Neptune Swap from Home to send a swap.".into());
    }
    if account.is_none() {
        return (false, "Connect elastOS Wallet first: the Base side runs through it.".into());
    }
    if offers_ui::taken_offer(app).is_none() {
        return (false, "Pick an open offer first: the swap goes sealed to that offer's solver.".into());
    }
    (true, "A one-time DID signs it, and it goes sealed to the offer's solver over the carrier. Nobody else can read it.".into())
}

fn deals_topic(main_net: bool) -> &'static str {
    if main_net { intents_protocol::DEALS_TOPIC_MAIN } else { intents_protocol::DEALS_TOPIC_TEST }
}

async fn send(weak: Weak<AppWindow>, state: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let token = state.borrow().token.clone();
    let account = state.borrow().account.borrow().clone();
    let (ready, why) = send_state(&app, account.as_ref(), token.is_some());
    if !ready || state.borrow().swaps.iter().any(Swap::open_ended) {
        app.set_form_error(if ready { "Finish the swap in progress first.".to_string() } else { why }.into());
        return;
    }
    let Some(solver) = offers_ui::reviewed_solver(&app) else {
        app.set_form_error("The offer or the amounts changed since the review: review the swap again.".into());
        app.set_review_open(false);
        return;
    };
    let (Some(network), Some(token)) = (wallet_ui::network(&app), token) else { return };
    let form = intent::Form {
        give: app.get_give_index() as usize,
        give_amount: &app.get_give_amount(),
        want: app.get_want_index() as usize,
        want_amount: &app.get_want_amount(),
        ttl: app.get_ttl_index() as usize,
    };
    let now = runtime::now_secs();
    let built = match intent::build(&form, intent::random_id(), now) {
        Ok(built) => built,
        Err(e) => return app.set_form_error(e.into()),
    };
    let mut seed = [0u8; 32];
    if getrandom::getrandom(&mut seed).is_err() {
        return app.set_form_error("No randomness for the one-time DID.".into());
    }
    let main_net = network == "neptune-main";
    if main_net {
        return app.set_form_error("Swaps run on test networks only until an independent audit. Pick a test network.".into());
    }
    drop(app);
    if let Some(ns) = evm_leg::namespace(built.other_chain()) {
        if let Err(e) = evm_leg::require_test_chain(ns, &token).await {
            if let Some(app) = weak.upgrade() {
                app.set_form_error(e.into());
            }
            return;
        }
    }
    let Some(app) = weak.upgrade() else { return };
    let (mut swap, sealed) = match Swap::start(built, &solver, main_net, seed, now) {
        Ok(started) => started,
        Err(e) => return app.set_form_error(e.into()),
    };
    app.set_review_open(false);
    drop(app);
    // Kept before it is sent: a swap the solver may answer is never lost.
    if let Err(e) = save(&token, &network, &swap).await {
        if let Some(app) = weak.upgrade() {
            app.set_form_error(format!("Could not keep the swap: {e}").into());
        }
        return;
    }
    if let Err(e) = deliver(&token, main_net, &sealed).await {
        swap.line = format!("Could not reach the carrier ({e}); trying again.");
    }
    swap.sent(&sealed, now);
    let _ = save(&token, &network, &swap).await;
    state.borrow_mut().swaps.push(swap);
    if let Some(app) = weak.upgrade() {
        render(&app, &state);
    }
    run(weak, state, network, token).await;
}

/// Picks up every open swap from the journal (after the page was closed).
async fn resume(weak: Weak<AppWindow>, state: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let (Some(network), Some(token)) = (wallet_ui::network(&app), state.borrow().token.clone()) else { return };
    drop(app);
    let list = match runtime::provider("swap_list", &json!({ "network": network }), &token).await {
        Ok(list) => list,
        Err(e) => {
            state.borrow_mut().trouble = Some(format!("Could not read your swaps: {}", e.message));
            return;
        }
    };
    let records = list["swaps"].as_object().cloned().unwrap_or_default();
    let mut unreadable = 0;
    let mut open: Vec<Swap> = vec![];
    let mut finished: Vec<Swap> = vec![];
    for (_, record) in records {
        match serde_json::from_value::<Swap>(record) {
            Ok(swap) if swap.open_ended() => open.push(swap),
            Ok(swap) => {
                if completed(&swap) {
                    offers_ui::trust(&swap.solver);
                }
                finished.push(swap);
            }
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        state.borrow_mut().trouble = Some(format!("{unreadable} kept swap record(s) could not be read by this version of the app."));
    }
    state.borrow_mut().history = finished;
    if let Some(app) = weak.upgrade() {
        render(&app, &state);
    }
    if open.is_empty() || state.borrow().running {
        return;
    }
    open.sort_by_key(|s| s.intent().expires_at);
    state.borrow_mut().swaps = open;
    if let Some(app) = weak.upgrade() {
        render(&app, &state);
    }
    run(weak, state, network, token).await;
}

async fn deliver(token: &str, main_net: bool, sealed: &[u8]) -> Result<(), String> {
    let topic = deals_topic(main_net);
    offers_ui::join(topic, token).await?;
    let message = URL_SAFE_NO_PAD.encode(sealed);
    runtime::peer("gossip_send", &json!({ "topic": topic, "message": message }), token).await.map_err(|e| e.message)?;
    Ok(())
}

/// Sends `sealed` and keeps it to send again while there is no answer.
async fn tell(token: &str, swap: &mut Swap, sealed: &[u8], now: u64) -> Result<(), String> {
    swap.sent(sealed, now);
    deliver(token, swap.main_net, sealed).await
}

/// Everything new on the deals topic (each read consumes it): shared by all open swaps.
async fn inbox(token: &str, main_net: bool) -> Result<Vec<Vec<u8>>, String> {
    let topic = deals_topic(main_net);
    offers_ui::join(topic, token).await?;
    let mut all = vec![];
    for _ in 0..10 {
        // What was read before an error is kept: each read consumes its messages.
        let Ok(reply) = runtime::peer("gossip_recv", &json!({ "topic": topic, "limit": 50 }), token).await else { break };
        let messages = reply["messages"].as_array().cloned().unwrap_or_default();
        all.extend(messages.iter().filter_map(|m| URL_SAFE_NO_PAD.decode(m["content"].as_str()?).ok()));
        if messages.len() < 50 {
            break;
        }
    }
    Ok(all)
}

async fn save(token: &str, network: &str, swap: &Swap) -> Result<(), String> {
    let record = serde_json::to_value(swap).map_err(|e| e.to_string())?;
    runtime::provider("swap_save", &json!({ "network": network, "id": swap.id, "record": record }), token).await.map_err(|e| e.message)?;
    Ok(())
}

async fn provider(op: &str, network: &str, mut body: Value, token: &str) -> Result<Value, String> {
    body["network"] = json!(network);
    runtime::provider(op, &body, token).await.map_err(|e| e.message)
}

/// The Wallet account for this swap's Base side: the one it was accepted with.
fn account_for(state: &Shared, swap: &Swap) -> Result<Account, String> {
    let account = state.borrow().account.borrow().clone().ok_or("Connect elastOS Wallet to continue this swap.")?;
    match swap.account() {
        Some(a) if !a.eq_ignore_ascii_case(&evm_htlc::hex(&account.address)[2..]) => {
            Err("Connect the Wallet account this swap was made with to continue.".into())
        }
        _ => Ok(account),
    }
}

fn namespace(swap: &Swap) -> Result<&'static str, String> {
    evm_leg::namespace(swap.intent().other_chain()).ok_or("elastOS Wallet cannot pay on that chain.".into())
}

enum Job {
    Proving,
    Sent,
    Failed(String),
    /// The provider no longer knows the job (it restarted): the chain decides.
    Gone,
}

async fn job(network: &str, job: &str, token: &str) -> Job {
    match provider("job_status", network, json!({ "job": job }), token).await {
        Ok(v) if v["state"] == "sent" => Job::Sent,
        Ok(v) if v["state"] == "failed" => Job::Failed(v["message"].as_str().unwrap_or("failed").to_string()),
        Ok(_) => Job::Proving,
        Err(_) => Job::Gone,
    }
}

/// The loop: every open swap, one step each per pass, until none is open. Every change is saved.
async fn run(weak: Weak<AppWindow>, state: Shared, network: String, token: String) {
    if std::mem::replace(&mut state.borrow_mut().running, true) {
        return;
    }
    loop {
        let swaps = state.borrow().swaps.clone();
        if !swaps.iter().any(Swap::open_ended) {
            break;
        }
        let now = runtime::now_secs();
        // Each EVM chain's own clock, read once a pass: every deadline on that chain is
        // checked against it.
        let mut clocks: Vec<(&'static str, Option<u64>)> = vec![];
        for ns in [evm_leg::BASE] {
            if swaps.iter().any(|s| s.open_ended() && namespace(s).ok() == Some(ns)) {
                clocks.push((ns, evm_leg::chain_time(ns, &token).await.ok()));
            }
        }
        // One read of the deals topic for all swaps of each network kind.
        let mut mail: Vec<Vec<u8>> = vec![];
        for main_net in [false, true] {
            if swaps.iter().any(|s| s.open_ended() && s.main_net == main_net) {
                mail.extend(inbox(&token, main_net).await.unwrap_or_default());
            }
        }
        let mut trouble = false;
        let mut updated = vec![];
        for mut swap in swaps {
            if !swap.open_ended() {
                updated.push(swap);
                continue;
            }
            let before = swap.clone();
            swap.tick(now);
            let chain_now = namespace(&swap).ok().and_then(|ns| clocks.iter().find(|(n, _)| *n == ns).and_then(|(_, t)| *t));
            if let Some(chain_now) = chain_now {
                swap.tick_chain(chain_now, now);
            }
            let mine: Vec<Vec<u8>> = mail.iter().filter_map(|m| swap.open(m)).collect();
            if let Err(e) = step(&weak, &state, &mut swap, &network, &token, &mine, chain_now, now).await {
                swap.line = e;
                trouble = true;
            }
            if let Some(again) = swap.resend_due(now, RESEND_SECS) {
                let _ = deliver(&token, swap.main_net, &again).await;
            }
            if before.open_ended() && completed(&swap) {
                offers_ui::trust(&swap.solver);
            }
            if swap != before {
                if let Err(e) = save(&token, &network, &swap).await {
                    swap.line = format!("Could not keep this swap's progress ({e}); trying again.");
                    trouble = true;
                }
            }
            updated.push(swap);
        }
        // Swaps added while this pass ran (a new send) stay in.
        let mut s = state.borrow_mut();
        let added: Vec<Swap> = s.swaps.iter().filter(|n| !updated.iter().any(|u| u.id == n.id)).cloned().collect();
        updated.extend(added);
        s.swaps = updated;
        drop(s);
        match weak.upgrade() {
            Some(app) => render(&app, &state),
            None => break,
        }
        runtime::sleep_ms(if trouble { RETRY_MS } else { POLL_MS }).await;
    }
    state.borrow_mut().running = false;
}

/// One pass over a swap's current step. `mine` are the messages sealed to it.
#[allow(clippy::too_many_arguments)]
async fn step(
    weak: &Weak<AppWindow>,
    state: &Shared,
    swap: &mut Swap,
    network: &str,
    token: &str,
    mine: &[Vec<u8>],
    chain_now: Option<u64>,
    now: u64,
) -> Result<(), String> {
    let base_clock = || chain_now.ok_or("Could not read Base's clock; trying again.".to_string());
    let waiting = |line: String| {
        if let Some(app) = weak.upgrade() {
            app.set_swap_line(line.into());
        }
    };
    match swap.step {
        Step::Asked => {
            for m in mine {
                if swap.on_quote(m, now).is_ok() {
                    break;
                }
            }
        }
        Step::Quoted => {
            evm_leg::require_test_chain(namespace(swap)?, token).await?;
            let account = account_for(state, swap)?;
            let key = provider("htlc_claim_lock", network, json!({}), token).await?;
            let claim_lock = key["claim_lock"].as_str().ok_or("no swap key")?.to_string();
            let index = key["index"].as_u64().ok_or("no swap key index")?;
            let prepared = if swap.gives_npt { Some(provider("htlc_prepare", network, json!({}), token).await?) } else { None };
            let prepared_ref = prepared.as_ref().and_then(|p| Some((p["hash_lock"].as_str()?, p["swap_id"].as_str()?)));
            if swap.gives_npt && prepared_ref.is_none() {
                return Err("The provider made no swap secret.".into());
            }
            let sealed = swap.accept(&claim_lock, index, prepared_ref, &account.address, now)?;
            tell(token, swap, &sealed, now).await?;
        }
        Step::Accepted => {
            let npt = intent::ASSETS.iter().find(|a| a.symbol == "NPT").ok_or("no NPT")?;
            let text = offers_ui::to_text(swap.intent().give_amount.units(), npt);
            let request = swap.npt_lock_request(&text, now)?;
            // The provider answers a repeated request for this swap with the same lock.
            let reply = provider("htlc_lock", network, request, token).await?;
            swap.npt_lock_started(&reply)?;
        }
        Step::LockingNpt => {
            let sent = match swap.job.clone() {
                Some(id) => match job(network, &id, token).await {
                    Job::Sent => true,
                    Job::Proving => false,
                    Job::Failed(e) => {
                        swap.npt_lock_failed(&e);
                        return Ok(());
                    }
                    Job::Gone => {
                        swap.job = None;
                        false
                    }
                },
                None => provider("htlc_status", network, json!({ "swap_id": swap.swap_id() }), token).await?["found"] == true,
            };
            if sent {
                let sealed = swap.npt_lock_sent(now)?;
                tell(token, swap, &sealed, now).await?;
            }
        }
        Step::AwaitEvmLock | Step::CheckEvmLock => {
            for m in mine {
                if swap.on_evm_locked(m, now).is_ok() {
                    break;
                }
            }
            if swap.step == Step::CheckEvmLock {
                let ns = namespace(swap)?;
                evm_leg::check_contract(ns, token).await?;
                let id = swap.evm_id().ok_or("no Base lock")?;
                let latest = evm_leg::latest_block(ns, token).await?;
                // About two days of Base blocks back: a page closed for a long while still finds it.
                swap.watch_from(latest.saturating_sub(evm_htlc::LOG_PAGE * 45));
                let view = evm_leg::lock_view(ns, &id, token).await?;
                // Confirmations from the lock's own block, not from when this page saw it.
                let locked_at = match swap.evm_lock_block {
                    Some(block) => block,
                    None => {
                        let block = evm_leg::locked_block(ns, &id, swap.evm_from_block(), token).await?.ok_or("The solver's Base lock is not on chain yet.")?;
                        swap.evm_lock_block = Some(block);
                        block
                    }
                };
                let _ = swap.check_evm_lock(&view, latest.saturating_sub(locked_at) + 1, base_clock()?);
            }
        }
        Step::Claiming => {
            let ns = namespace(swap)?;
            let id = swap.evm_id().ok_or("no Base lock")?;
            let view = evm_leg::lock_view(ns, &id, token).await?;
            if view.spent {
                // Spent by a claim (a Claimed log under this hash lock) or by the solver's refund.
                let hash_lock = swap.hash_lock().ok_or("no hash lock")?;
                if evm_leg::claimed_secret(ns, &id, &hash_lock, swap.evm_from_block(), token).await?.is_some() {
                    // Done once the claim is deep enough that no re-org can undo it.
                    let at = evm_leg::claimed_block(ns, &id, swap.evm_from_block(), token).await?.ok_or("Reading the claim's block; trying again.")?;
                    let depth = evm_leg::latest_block(ns, token).await?.saturating_sub(at) + 1;
                    let needed = swap.intent().other_chain().confirmations();
                    if depth >= needed {
                        swap.finish(format!("Swap complete: {} is in your Wallet account.", swap.want_label()));
                    } else {
                        swap.line = format!("Claimed on Base; waiting for confirmations ({depth}/{needed}).");
                    }
                } else if swap.claim_sent && !evm_leg::refunded(ns, &id, swap.evm_from_block(), token).await? {
                    // Our claim went out and the lock is spent, but no log yet: the logs lag.
                    return Err("Claimed on Base; waiting for the chain's logs.".into());
                } else {
                    // Refunded by the solver: wait for the NPT refund at its timeout.
                    swap.step = Step::CheckEvmLock;
                    swap.line = "The solver took its Base lock back. Your NPT comes back after its timeout.".into();
                }
                return Ok(());
            }
            let account = account_for(state, swap)?;
            let secret = provider("htlc_secret", network, json!({ "swap_id": swap.swap_id() }), token).await?;
            let secret = secret["secret"].as_str().and_then(evm_htlc::parse_word).ok_or("the provider has no secret")?;
            let tx = swap.claim_tx(&secret)?;
            // Re-checked right before the secret leaves (nothing slow after this); kept as sent
            // before it is sent.
            if !swap.claim_sent {
                let view = evm_leg::lock_view(ns, &id, token).await?;
                // Too late to reveal? The later of Base's clock and the computer's.
                let chain_time = evm_leg::chain_time(ns, token).await?;
                swap.ready_to_reveal(&view, chain_time.max(runtime::now_secs()))?;
                save(token, network, swap).await?;
            }
            evm_leg::send(ns, &account, &tx, token, &waiting).await?;
            // Counted done once the claim is deep enough (the spent path above).
            swap.line = "Claimed on Base; waiting for confirmations.".into();
        }
        Step::AwaitNptLock | Step::CheckNptLock => {
            if swap.step == Step::AwaitNptLock {
                for m in mine {
                    if let Ok(request) = swap.on_npt_locked(m, now) {
                        let reply = provider("htlc_check", network, request, token).await?;
                        swap.npt_checked(&reply, base_clock()?.max(now))?;
                        break;
                    }
                }
            } else {
                let reply = provider("htlc_check", network, swap.npt_check_request()?, token).await?;
                swap.npt_checked(&reply, base_clock()?.max(now))?;
            }
        }
        Step::LockingEvm => {
            let ns = namespace(swap)?;
            evm_leg::check_contract(ns, token).await?;
            let id = swap.planned_evm_id()?;
            // Landed already (the page closed before it could say so)?
            if evm_leg::lock_view(ns, &id, token).await?.sender != [0; 20] {
                let sealed = swap.evm_lock_sent(swap.evm_from_block(), now)?;
                tell(token, swap, &sealed, now).await?;
                return Ok(());
            }
            if swap.lock_broadcast {
                // Sent before: never send again; watch until its refund time, when a lock that
                // has not landed cannot land any more.
                if swap.evm_refund_after().is_some_and(|at| base_clock().is_ok_and(|t| t > at)) {
                    swap.step = Step::Failed;
                    swap.line = "Your Base lock never landed; nothing of yours was locked. The solver's NPT goes back to it.".into();
                } else {
                    swap.line = "Waiting for your Base lock to land…".into();
                }
                return Ok(());
            }
            evm_leg::require_test_chain(ns, token).await?;
            if !swap.evm_lock_in_time(base_clock()?.max(now)) {
                swap.step = Step::Failed;
                swap.line = "Too late to lock on Base safely; nothing of yours was locked there. The solver's NPT goes back to it.".into();
                return Ok(());
            }
            let account = account_for(state, swap)?;
            // Where to look for the solver's claim from, kept before the lock goes out.
            swap.watch_from(evm_leg::latest_block(ns, token).await?);
            swap.lock_broadcast = true;
            save(token, network, swap).await?;
            let mut block = 0;
            for tx in swap.evm_lock_txs()? {
                block = evm_leg::send(ns, &account, &tx, token, &waiting).await?.block;
            }
            let sealed = swap.evm_lock_sent(block, now)?;
            tell(token, swap, &sealed, now).await?;
        }
        Step::AwaitSecret | Step::RefundingEvm => {
            let ns = namespace(swap)?;
            let (id, hash_lock) = (swap.evm_id().ok_or("no Base lock")?, swap.hash_lock().ok_or("no hash lock")?);
            // Always look for the secret first: a claim in the last block before the refund
            // time still pays the solver, so the NPT must be claimed.
            if let Some(secret) = evm_leg::claimed_secret(ns, &id, &hash_lock, swap.evm_from_block(), token).await? {
                let request = swap.npt_claim_request(&secret)?;
                let reply = provider("htlc_claim", network, request, token).await?;
                swap.job = reply["job"].as_str().map(str::to_string);
                swap.submitted_at = Some(now);
                return Ok(());
            }
            if swap.step == Step::RefundingEvm {
                if evm_leg::lock_view(ns, &id, token).await?.spent {
                    // Spent: finished only with a Refunded log; otherwise a claim's log is late
                    // and the secret is looked for again.
                    if evm_leg::refunded(ns, &id, swap.evm_from_block(), token).await? {
                        swap.finish(format!("{} is back in your Wallet account. The swap did not happen.", swap.give_label()));
                        return Ok(());
                    }
                    return Err("The Base lock is spent; reading the chain again for the secret.".into());
                }
                let account = account_for(state, swap)?;
                evm_leg::send(ns, &account, &swap.evm_refund_tx()?, token, &waiting).await?;
                swap.finish(format!("{} is back in your Wallet account. The swap did not happen.", swap.give_label()));
            }
        }
        Step::ClaimingNpt => {
            if let Some(id) = swap.job.clone() {
                match job(network, &id, token).await {
                    Job::Proving => return Ok(()),
                    Job::Sent | Job::Gone => swap.job = None,
                    Job::Failed(e) => {
                        swap.job = None;
                        swap.submitted_at = None;
                        swap.line = format!("The NPT claim failed ({e}); trying again.");
                    }
                }
            }
            let note = swap.npt_note().ok_or("no lock note")?;
            let spent = provider("htlc_spent", network, json!({ "note": note, "from_height": swap.npt_from_height() }), token).await?;
            if spent["spent"] == true {
                let depth = spent["depth"].as_u64().unwrap_or(0);
                if spent["by_claim"] != true {
                    swap.finish("The NPT lock was refunded to its giver, not claimed by you. Check your Neptune wallet.");
                } else if depth >= NPT_SETTLED_DEPTH {
                    swap.finish(format!("Swap complete: {} is in your Neptune wallet.", swap.want_label()));
                } else {
                    swap.line = format!("Your NPT claim is in a block; waiting for confirmations ({depth}/{NPT_SETTLED_DEPTH}).");
                }
                return Ok(());
            }
            if swap.submitted_at.is_none_or(|at| now >= at + RESUBMIT_SECS) {
                // Claim (again): the secret is public on Base.
                let ns = namespace(swap)?;
                let (id, hash_lock) = (swap.evm_id().ok_or("no Base lock")?, swap.hash_lock().ok_or("no hash lock")?);
                let secret = evm_leg::claimed_secret(ns, &id, &hash_lock, swap.evm_from_block(), token).await?.ok_or("The secret is not on Base yet.")?;
                let request = swap.npt_claim_request(&secret)?;
                let reply = provider("htlc_claim", network, request, token).await?;
                swap.job = reply["job"].as_str().map(str::to_string);
                swap.submitted_at = Some(now);
            }
        }
        Step::RefundingNpt => {
            if let Some(id) = swap.job.clone() {
                match job(network, &id, token).await {
                    Job::Proving => return Ok(()),
                    Job::Sent | Job::Gone => swap.job = None,
                    Job::Failed(e) => {
                        swap.job = None;
                        swap.submitted_at = None;
                        swap.line = format!("The NPT refund failed ({e}); trying again.");
                    }
                }
            }
            let spent = provider("htlc_spent", network, json!({ "swap_id": swap.swap_id() }), token).await?;
            if spent["spent"] == true {
                let depth = spent["depth"].as_u64().unwrap_or(0);
                if depth >= NPT_SETTLED_DEPTH {
                    swap.finish("Your NPT is back in your wallet. The swap did not happen.");
                } else {
                    swap.line = format!("Your NPT refund is in a block; waiting for confirmations ({depth}/{NPT_SETTLED_DEPTH}).");
                }
                return Ok(());
            }
            // A lock that never reached the chain: decided only when this Home's node has blocks
            // from well past the refund time (a node behind or syncing knows too little).
            let tip_at = spent["tip_at_ms"].as_u64().unwrap_or(0);
            if spent["found"] == false && swap.neptune_refund_ms().is_some_and(|ms| tip_at > ms + NPT_NEVER_LANDED_MS) {
                swap.finish("Your NPT lock never reached the chain; your NPT stayed in your wallet.");
                return Ok(());
            }
            if swap.submitted_at.is_none_or(|at| now >= at + RESUBMIT_SECS) {
                let reply = provider("htlc_refund", network, swap.npt_refund_request()?, token).await?;
                swap.job = reply["job"].as_str().map(str::to_string);
                swap.submitted_at = Some(now);
            }
        }
        Step::Done | Step::Failed => {}
    }
    Ok(())
}

/// A swap that finished with both sides paid.
fn completed(swap: &Swap) -> bool {
    swap.step == Step::Done && swap.line.starts_with("Swap complete")
}

/// The steps the card lists for this direction, and where the swap is among them.
fn steps(swap: &Swap) -> (Vec<&'static str>, usize) {
    if swap.gives_npt {
        let names = vec!["Quote from the solver", "Accept", "Lock your NPT", "Solver locks on Base", "Claim on Base"];
        let at = match swap.step {
            Step::Asked => 0,
            Step::Quoted => 1,
            Step::Accepted | Step::LockingNpt => 2,
            Step::AwaitEvmLock | Step::CheckEvmLock | Step::RefundingNpt => 3,
            Step::Claiming => 4,
            _ => 5,
        };
        (names, at)
    } else {
        let names = vec!["Quote from the solver", "Accept", "Solver locks NPT", "Lock on Base", "Solver claims (the secret goes public)", "Claim your NPT"];
        let at = match swap.step {
            Step::Asked => 0,
            Step::Quoted => 1,
            Step::AwaitNptLock | Step::CheckNptLock => 2,
            Step::LockingEvm => 3,
            Step::AwaitSecret | Step::RefundingEvm => 4,
            Step::ClaimingNpt => 5,
            _ => 6,
        };
        (names, at)
    }
}

/// Finished swaps, newest first, except the one the card shows.
fn history_rows(s: &Ctx) -> Vec<AddressRow> {
    let shown = s.swaps.last().map(|w| w.id.clone());
    // The live copy of a swap wins over an older one kept in the journal.
    let mut seen = std::collections::HashSet::new();
    let mut done: Vec<&Swap> = s
        .swaps
        .iter()
        .filter(|w| !w.open_ended())
        .chain(s.history.iter())
        .filter(|w| Some(&w.id) != shown.as_ref() && seen.insert(w.id.clone()))
        .collect();
    done.sort_by_key(|w| std::cmp::Reverse(w.when()));
    done.iter()
        .map(|w| {
            let status = if completed(w) { "Completed".to_string() } else { w.line.clone() };
            let when = runtime::format_time(w.when().saturating_mul(1000));
            AddressRow {
                title: format!("{} → {}", w.give_label(), w.want_label()).into(),
                detail: format!("{when} · {status}").into(),
                current: completed(w),
            }
        })
        .collect()
}

fn render(app: &AppWindow, state: &Shared) {
    let s = state.borrow();
    app.set_swap_history(ModelRc::new(VecModel::from(history_rows(&s))));
    let Some(swap) = s.swaps.last() else {
        app.set_swap_on(false);
        return;
    };
    let (names, at) = steps(swap);
    let failed = swap.step == Step::Failed;
    let rows: Vec<StepRow> = names
        .iter()
        .enumerate()
        .map(|(i, name)| StepRow {
            text: SharedString::from(*name),
            state: SharedString::from(match (i.cmp(&at), failed) {
                (std::cmp::Ordering::Less, _) => "done",
                (std::cmp::Ordering::Equal, true) => "failed",
                (std::cmp::Ordering::Equal, false) => "now",
                _ => "todo",
            }),
        })
        .collect();
    let others = s.swaps.iter().filter(|w| w.open_ended()).count().saturating_sub(usize::from(swap.open_ended()));
    let mut line = swap.line.clone();
    if others > 0 {
        line = format!("{line} ({others} more swap(s) running.)");
    }
    if let Some(trouble) = &s.trouble {
        line = format!("{line} {trouble}");
    }
    app.set_swap_on(true);
    app.set_swap_title(format!("{} → {}", swap.give_label(), swap.want_label()).into());
    app.set_swap_line(line.into());
    app.set_swap_finished(!swap.open_ended());
    app.set_swap_steps(ModelRc::new(VecModel::from(rows)));
}

/// Development only: the swap card in a given state, for screenshots.
pub fn demo(app: &AppWindow, which: &str) {
    let rows = |at: usize, names: &[&str]| -> Vec<StepRow> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| StepRow { text: (*n).into(), state: (if i < at { "done" } else if i == at { "now" } else { "todo" }).into() })
            .collect()
    };
    app.set_swap_on(true);
    app.set_page(3);
    if which == "swap-get" {
        app.set_swap_title("250 USDC → 1,600 NPT".into());
        app.set_swap_line("Locked on Base. Waiting for the solver to claim it, which reveals the secret.".into());
        let names = ["Quote from the solver", "Accept", "Solver locks NPT", "Lock on Base", "Solver claims (the secret goes public)", "Claim your NPT"];
        app.set_swap_steps(ModelRc::new(VecModel::from(rows(4, &names))));
    } else {
        app.set_swap_title("1,600 NPT → 250 USDC".into());
        app.set_swap_line("The solver's lock is good; waiting for confirmations (4/10).".into());
        let names = ["Quote from the solver", "Accept", "Lock your NPT", "Solver locks on Base", "Claim on Base"];
        app.set_swap_steps(ModelRc::new(VecModel::from(rows(3, &names))));
    }
}
