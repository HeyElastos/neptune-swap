//! Runs a swap from the page: carries out what each step of the engine (`swap.rs`) needs
//! (provider calls, Wallet transactions, chain reads, sealed messages on the deals topic),
//! feeds the answers back, keeps the swap in the provider's journal after every change, and
//! shows it in the swap card. One swap at a time; an open one is picked up again when the
//! page opens.

use std::cell::RefCell;
use std::rc::Rc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::evm_leg::{self, Account};
use crate::swap::{Step, Swap};
use crate::{intent, offers_ui, runtime, wallet_ui, AppWindow, StepRow};

/// How often the swap looks for news while it waits.
const POLL_MS: u32 = 5_000;
/// Longer pause after an error, before trying the step again.
const RETRY_MS: u32 = 30_000;

struct Ctx {
    token: Option<String>,
    account: Rc<RefCell<Option<Account>>>,
    swap: Option<Swap>,
    running: bool,
}

type Shared = Rc<RefCell<Ctx>>;

pub fn wire(app: &AppWindow, token: Option<String>, account: Rc<RefCell<Option<Account>>>) {
    let state: Shared = Rc::new(RefCell::new(Ctx { token, account, swap: None, running: false }));
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_send_swap(move || crate::spawn(send(weak.clone(), state.clone())));
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_swap_dismiss(move || {
            let mut s = state.borrow_mut();
            if s.swap.as_ref().is_some_and(|w| !w.open_ended()) {
                s.swap = None;
                weak.unwrap().set_swap_on(false);
            }
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
    if !ready || state.borrow().swap.as_ref().is_some_and(Swap::open_ended) {
        app.set_form_error(if ready { "Finish the swap in progress first.".to_string() } else { why }.into());
        return;
    }
    let (Some(network), Some(solver), Some(token)) = (wallet_ui::network(&app), offers_ui::taken_offer(&app), token) else { return };
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
    let (swap, sealed) = match Swap::start(built, &solver, main_net, seed, now) {
        Ok(started) => started,
        Err(e) => return app.set_form_error(e.into()),
    };
    app.set_review_open(false);
    drop(app);
    if let Err(e) = deliver(&token, main_net, &sealed).await {
        if let Some(app) = weak.upgrade() {
            app.set_form_error(format!("Could not reach the carrier: {e}").into());
        }
        return;
    }
    let _ = save(&token, &network, &swap).await;
    state.borrow_mut().swap = Some(swap);
    if let Some(app) = weak.upgrade() {
        render(&app, &state);
    }
    run(weak, state, network, token).await;
}

/// Picks up the newest open swap from the journal (after the page was closed).
async fn resume(weak: Weak<AppWindow>, state: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let (Some(network), Some(token)) = (wallet_ui::network(&app), state.borrow().token.clone()) else { return };
    drop(app);
    if state.borrow().running {
        return;
    }
    let Ok(list) = runtime::provider("swap_list", &json!({ "network": network }), &token).await else { return };
    let mut open: Vec<Swap> = list["swaps"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| serde_json::from_value::<Swap>(v.clone()).ok())
        .filter(Swap::open_ended)
        .collect();
    open.sort_by_key(|s| std::cmp::Reverse(s.intent().expires_at));
    let Some(swap) = open.into_iter().next() else { return };
    state.borrow_mut().swap = Some(swap);
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

/// The messages on the deals topic sealed to this swap, opened.
async fn inbox(token: &str, swap: &Swap) -> Result<Vec<Vec<u8>>, String> {
    let topic = deals_topic(swap.main_net);
    offers_ui::join(topic, token).await?;
    let mut opened = vec![];
    for _ in 0..10 {
        let reply = runtime::peer("gossip_recv", &json!({ "topic": topic, "limit": 50 }), token).await.map_err(|e| e.message)?;
        let messages = reply["messages"].as_array().cloned().unwrap_or_default();
        for m in &messages {
            let Some(bytes) = m["content"].as_str().and_then(|c| URL_SAFE_NO_PAD.decode(c).ok()) else { continue };
            if let Some(plain) = swap.open(&bytes) {
                opened.push(plain);
            }
        }
        if messages.len() < 50 {
            break;
        }
    }
    Ok(opened)
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

/// Where a provider job stands: Some(Ok) when sent, Some(Err) when it failed, None while proving.
async fn job(network: &str, job: &str, token: &str) -> Option<Result<(), String>> {
    match provider("job_status", network, json!({ "job": job }), token).await {
        Ok(v) if v["state"] == "sent" => Some(Ok(())),
        Ok(v) if v["state"] == "failed" => Some(Err(v["message"].as_str().unwrap_or("failed").to_string())),
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

/// The loop: one step at a time until the swap is done or failed. Every change is saved.
async fn run(weak: Weak<AppWindow>, state: Shared, network: String, token: String) {
    if std::mem::replace(&mut state.borrow_mut().running, true) {
        return;
    }
    // Base blocks when the solver's lock was first seen good (not kept: a reload counts again).
    let mut seen_at: Option<u64> = None;
    loop {
        let Some(mut swap) = state.borrow().swap.clone() else { break };
        let before = swap.clone();
        let now = runtime::now_secs();
        swap.tick(now);
        let outcome = step(&weak, &state, &mut swap, &network, &token, &mut seen_at, now).await;
        if let Err(e) = &outcome {
            swap.line = e.clone();
        }
        if swap != before {
            let _ = save(&token, &network, &swap).await;
        }
        let open = swap.open_ended();
        state.borrow_mut().swap = Some(swap);
        match weak.upgrade() {
            Some(app) => render(&app, &state),
            None => break,
        }
        if !open {
            break;
        }
        runtime::sleep_ms(if outcome.is_err() { RETRY_MS } else { POLL_MS }).await;
    }
    state.borrow_mut().running = false;
}

/// One pass over the current step.
#[allow(clippy::too_many_arguments)]
async fn step(
    weak: &Weak<AppWindow>,
    state: &Shared,
    swap: &mut Swap,
    network: &str,
    token: &str,
    seen_at: &mut Option<u64>,
    now: u64,
) -> Result<(), String> {
    let waiting = |line: String| {
        if let Some(app) = weak.upgrade() {
            app.set_swap_line(line.into());
        }
    };
    match swap.step {
        Step::Asked => {
            for m in inbox(token, swap).await? {
                if swap.on_quote(&m, now).is_ok() {
                    break;
                }
            }
        }
        Step::Quoted => {
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
            deliver(token, swap.main_net, &sealed).await?;
        }
        Step::Accepted => {
            let npt = intent::ASSETS.iter().find(|a| a.symbol == "NPT").ok_or("no NPT")?;
            let text = offers_ui::to_text(swap.intent().give_amount.units(), npt);
            let request = swap.npt_lock_request(&text, now)?;
            let reply = provider("htlc_lock", network, request, token).await?;
            swap.npt_lock_started(&reply)?;
        }
        Step::LockingNpt => {
            let done = match swap.job.clone() {
                Some(id) => job(network, &id, token).await,
                // The job is gone (the provider restarted): is the lock on chain?
                None => {
                    let found = provider("htlc_status", network, json!({ "swap_id": swap.swap_id() }), token).await?;
                    (found["found"] == true).then_some(Ok(()))
                }
            };
            match done {
                Some(Ok(())) => {
                    let sealed = swap.npt_lock_sent(now)?;
                    deliver(token, swap.main_net, &sealed).await?;
                }
                Some(Err(e)) if swap.job.is_some() => {
                    swap.job = None;
                    return Err(format!("The NPT lock was not sent ({e}). Checking the chain again."));
                }
                _ => {}
            }
        }
        Step::AwaitEvmLock => {
            for m in inbox(token, swap).await? {
                if swap.on_evm_locked(&m, now).is_ok() {
                    break;
                }
            }
        }
        Step::CheckEvmLock => {
            let ns = namespace(swap)?;
            evm_leg::check_contract(ns, token).await?;
            let id = swap.evm_id().ok_or("no Base lock")?;
            let view = evm_leg::lock_view(ns, &id, token).await?;
            let latest = evm_leg::latest_block(ns, token).await?;
            let first = *seen_at.get_or_insert(latest);
            if swap.check_evm_lock(&view, latest.saturating_sub(first), now).is_err() {
                *seen_at = None;
            }
        }
        Step::Claiming => {
            let ns = namespace(swap)?;
            let id = swap.evm_id().ok_or("no Base lock")?;
            if evm_leg::lock_view(ns, &id, token).await?.spent {
                // Spent by a claim (a Claimed log under this hash lock) or by the solver's refund.
                let hash_lock = swap.hash_lock().ok_or("no hash lock")?;
                let from = evm_leg::latest_block(ns, token).await?.saturating_sub(evm_leg::LOG_PAGE * 5);
                if evm_leg::claimed_secret(ns, &id, &hash_lock, from, token).await?.is_some() {
                    swap.finish(format!("Swap complete: {} is in your Wallet account.", swap.want_label()));
                } else {
                    swap.step = Step::RefundingNpt;
                    swap.line = "The solver took its Base lock back. Taking your NPT back after its timeout.".into();
                }
                return Ok(());
            }
            let account = account_for(state, swap)?;
            let secret = provider("htlc_secret", network, json!({ "swap_id": swap.swap_id() }), token).await?;
            let secret = secret["secret"].as_str().and_then(evm_htlc::parse_word).ok_or("the provider has no secret")?;
            let tx = swap.claim_tx(&secret)?;
            evm_leg::send(ns, &account, &tx, token, &waiting).await?;
            swap.finish(format!("Swap complete: {} is in your Wallet account.", swap.want_label()));
        }
        Step::AwaitNptLock => {
            for m in inbox(token, swap).await? {
                if let Ok(request) = swap.on_npt_locked(&m, now) {
                    let reply = provider("htlc_check", network, request, token).await?;
                    swap.npt_checked(&reply, now)?;
                    break;
                }
            }
        }
        Step::CheckNptLock => {
            let reply = provider("htlc_check", network, swap.npt_check_request()?, token).await?;
            swap.npt_checked(&reply, now)?;
        }
        Step::LockingEvm => {
            let ns = namespace(swap)?;
            evm_leg::check_contract(ns, token).await?;
            let account = account_for(state, swap)?;
            let id = swap.planned_evm_id()?;
            // Landed already (the page closed before it could say so)?
            if evm_leg::lock_view(ns, &id, token).await?.sender != [0; 20] {
                let from = evm_leg::latest_block(ns, token).await?.saturating_sub(evm_leg::LOG_PAGE * 5);
                let sealed = swap.evm_lock_sent(from, now)?;
                deliver(token, swap.main_net, &sealed).await?;
                return Ok(());
            }
            let mut block = 0;
            for tx in swap.evm_lock_txs()? {
                block = evm_leg::send(ns, &account, &tx, token, &waiting).await?.block;
            }
            let sealed = swap.evm_lock_sent(block, now)?;
            deliver(token, swap.main_net, &sealed).await?;
        }
        Step::AwaitSecret => {
            let ns = namespace(swap)?;
            let (id, hash_lock) = (swap.evm_id().ok_or("no Base lock")?, swap.hash_lock().ok_or("no hash lock")?);
            if let Some(secret) = evm_leg::claimed_secret(ns, &id, &hash_lock, swap.evm_from_block(), token).await? {
                let request = swap.npt_claim_request(&secret)?;
                let reply = provider("htlc_claim", network, request, token).await?;
                swap.job = reply["job"].as_str().map(str::to_string);
            }
        }
        Step::ClaimingNpt => match swap.job.clone() {
            Some(id) => match job(network, &id, token).await {
                Some(Ok(())) => swap.finish(format!("Swap complete: {} is on its way to your Neptune wallet.", swap.want_label())),
                Some(Err(e)) => {
                    swap.job = None;
                    return Err(format!("The NPT claim failed ({e}); trying again."));
                }
                None => {}
            },
            None => {
                // Claim again: the secret is public on Base.
                let ns = namespace(swap)?;
                let (id, hash_lock) = (swap.evm_id().ok_or("no Base lock")?, swap.hash_lock().ok_or("no hash lock")?);
                let secret = evm_leg::claimed_secret(ns, &id, &hash_lock, swap.evm_from_block(), token).await?.ok_or("The secret is not on Base yet.")?;
                let request = swap.npt_claim_request(&secret)?;
                let reply = provider("htlc_claim", network, request, token).await?;
                swap.job = reply["job"].as_str().map(str::to_string);
            }
        },
        Step::RefundingNpt => match swap.job.clone() {
            Some(id) => match job(network, &id, token).await {
                Some(Ok(())) => swap.finish("Your NPT is back in your wallet. The swap did not happen."),
                Some(Err(e)) => {
                    swap.job = None;
                    return Err(format!("The NPT refund failed ({e}); trying again."));
                }
                None => {}
            },
            None => {
                let reply = provider("htlc_refund", network, swap.npt_refund_request()?, token).await?;
                swap.job = reply["job"].as_str().map(str::to_string);
            }
        },
        Step::RefundingEvm => {
            let ns = namespace(swap)?;
            let id = swap.evm_id().ok_or("no Base lock")?;
            if evm_leg::lock_view(ns, &id, token).await?.spent {
                // Spent: the solver claimed after all (then the secret is public), or refunded.
                swap.finish("The Base lock is settled. Check your Wallet and Neptune wallet.");
                return Ok(());
            }
            let account = account_for(state, swap)?;
            evm_leg::send(ns, &account, &swap.evm_refund_tx()?, token, &waiting).await?;
            swap.finish(format!("{} is back in your Wallet account. The swap did not happen.", swap.give_label()));
        }
        Step::Done | Step::Failed => {}
    }
    Ok(())
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

fn render(app: &AppWindow, state: &Shared) {
    let s = state.borrow();
    let Some(swap) = &s.swap else {
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
    app.set_swap_on(true);
    app.set_swap_title(format!("{} → {}", swap.give_label(), swap.want_label()).into());
    app.set_swap_line(swap.line.clone().into());
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
