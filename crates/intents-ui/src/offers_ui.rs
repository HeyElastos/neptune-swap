//! The offers on screen: the open list under the swap form (heard on the
//! carrier), and this Home's own offers as a solver (signed by the provider,
//! published on the carrier, republished while the page is open).

use std::cell::RefCell;
use std::rc::Rc;

use intents_protocol::{Amount, Net, Offer};
use serde_json::json;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::intent::{AssetDef, ASSETS};
use crate::offers::{self, Board, SolverForm};
use crate::wallet_ui::network;
use crate::{runtime, spawn, AddressRow, AppWindow};

/// A copy of an offer lives this long; the page republishes every
/// `REPUBLISH_SECS`, so an offer ends soon after the page closes.
const COPY_SECS: u64 = 20 * 60;
const REPUBLISH_SECS: u32 = 10 * 60;
const OTHERS: [&str; 3] = ["USDC", "ETH", "BTC"];

/// The most offers this Home publishes at once (every reader keeps 5 per solver).
const MINE_MAX: usize = 5;
/// Messages read per call, and calls per poll: a flood cannot keep the list behind.
const RECV_BATCH: usize = 50;
const RECV_CALLS: usize = 10;

/// The form filled from an offer: that offer exactly as the user saw it,
/// and the amounts the form held then.
struct Filled {
    listed: offers::Listed,
    give_text: String,
    want_text: String,
}

#[derive(Default)]
struct State {
    token: Option<String>,
    /// Listening for offers (from the start, inside Home).
    listening: bool,
    /// The user typed what they want by hand: the form stops following the best offer until
    /// the pair changes.
    manual: bool,
    /// The pair the form showed last (give row, want row).
    pair: (i32, i32),
    board: Board,
    /// The topic joined, and the network it belongs to.
    joined: Option<(String, Net)>,
    /// The rows shown, exactly as shown.
    shown: Vec<offers::Listed>,
    filled: Option<Filled>,
    /// What the review sheet showed when it opened: Send goes out only while the form and the
    /// offer are still exactly that.
    reviewed: Option<Reviewed>,
    /// This Home's live offers.
    mine: Vec<Offer>,
    did: String,
}

type Shared = Rc<RefCell<State>>;

#[derive(Clone, PartialEq, Eq)]
struct Reviewed {
    solver: String,
    form: (i32, String, i32, String, i32),
}

fn snapshot(app: &AppWindow) -> Option<Reviewed> {
    let form = (app.get_give_index(), app.get_give_amount().to_string(), app.get_want_index(), app.get_want_amount().to_string(), app.get_ttl_index());
    Some(Reviewed { solver: taken_offer(app)?, form })
}

/// Called as the review sheet opens: fixes the solver and the form it shows.
pub fn freeze(app: &AppWindow) {
    let now = snapshot(app);
    STATE.with(|s| {
        if let Some(state) = s.borrow().as_ref() {
            state.borrow_mut().reviewed = now;
        }
    });
}

/// The solver to send to: only while the form and the offer are exactly what was reviewed.
pub fn reviewed_solver(app: &AppWindow) -> Option<String> {
    let now = snapshot(app)?;
    let same = STATE.with(|s| s.borrow().as_ref().is_some_and(|state| state.borrow().reviewed.as_ref() == Some(&now)));
    same.then_some(now.solver)
}

thread_local! {
    static STATE: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

fn net_of(network_id: &str) -> (Net, &'static str) {
    if network_id == "neptune-main" {
        (Net::Main, intents_protocol::OFFERS_TOPIC_MAIN)
    } else {
        (Net::Test, intents_protocol::OFFERS_TOPIC_TEST)
    }
}


/// `units` as the form's decimal text (rounded down to what it can hold).
pub fn to_text(units: u128, d: &AssetDef) -> String {
    let steps = units / d.unit;
    let scale = 10u128.pow(d.decimals);
    let (int, frac) = (steps / scale, steps % scale);
    if frac == 0 {
        return int.to_string();
    }
    let frac = format!("{frac:0width$}", width = d.decimals as usize);
    format!("{int}.{}", frac.trim_end_matches('0'))
}

pub fn wire(app: &AppWindow, token: Option<String>) {
    let state: Shared = Rc::new(RefCell::new(State { token: token.clone(), ..Default::default() }));
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
    {
        let (weak, state) = (app.as_weak(), state.clone());
        // A row tapped by hand stays: the form stops following the best offer until the pair
        // changes or "Find best offer" is pressed.
        app.on_take_offer(move |row| {
            if take(&weak.unwrap(), &state, row as usize, false) {
                state.borrow_mut().manual = true;
            }
        });
    }
    {
        // Fetch the best offer now: read the topic, follow the market again, take the best.
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_best_offer(move || {
            let (weak, state) = (weak.clone(), state.clone());
            spawn(async move {
                {
                    let mut s = state.borrow_mut();
                    s.manual = false;
                    s.filled = None;
                }
                let Some(app) = weak.upgrade() else { return };
                app.set_offers_status("Looking for the best offer…".into());
                let network_id = network(&app);
                drop(app);
                if let Some(network_id) = network_id {
                    let (net, topic) = net_of(&network_id);
                    if let Err(message) = hear(&state, net, topic).await {
                        if let Some(app) = weak.upgrade() {
                            app.set_offers_status(message.into());
                        }
                        return;
                    }
                }
                if let Some(app) = weak.upgrade() {
                    show(&app, &state);
                }
            });
        });
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_publish_offer(move || spawn(publish(weak.clone(), state.clone())));
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_withdraw_offer(move |row| spawn(withdraw(weak.clone(), state.clone(), row as usize)));
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_show_offers(move || {
            if state.borrow().listening {
                return;
            }
            state.borrow_mut().listening = true;
            let app = weak.unwrap();
            app.set_offers_on(true);
            app.set_offers_status("Listening for offers on the carrier…".into());
            spawn(listen(weak.clone(), state.clone()));
        });
    }
    if token.is_some() {
        // The market is read from the start: the form fills itself from the best offer.
        state.borrow_mut().listening = true;
        app.set_offers_on(true);
        app.set_offers_status("Listening for offers on the carrier…".into());
        spawn(listen(app.as_weak(), state.clone()));
        spawn(republish(app.as_weak(), state));
    } else {
        app.set_offers_status("Open Neptune Swap from Home to see offers.".into());
    }
}

/// The swap form changed: a form filled from an offer follows the offer's
/// rate when the amount given changes, and stops following it when the
/// amount asked is edited by hand.
pub fn form_changed(app: &AppWindow) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else { return };
    let mut s = state.borrow_mut();
    let Some(filled) = s.filled.as_mut() else { return };
    let (give_text, want_text) = (app.get_give_amount().to_string(), app.get_want_amount().to_string());
    if give_text == filled.give_text && want_text == filled.want_text {
        return;
    }
    let offer = filled.listed.offer.clone();
    let (Some(pay_i), Some(get_i)) = (crate::intent::row_of(&offer.want), crate::intent::row_of(&offer.give)) else { return };
    let same_pair = app.get_give_index() as usize == pay_i && app.get_want_index() as usize == get_i;
    if !same_pair || want_text != filled.want_text {
        // Asked by hand: stop following offers (a new pair follows the market again).
        s.manual = same_pair;
        s.filled = None;
        drop(s);
        app.set_offers_status(if same_pair { "The form no longer follows an offer." } else { "Looking for the best offer for this pair…" }.into());
        show(app, &state);
        return;
    }
    let paid = Amount::parse_decimal(&give_text, ASSETS[pay_i].decimals, ASSETS[pay_i].unit).ok();
    match paid.and_then(|p| offer.amount_for(p)) {
        Some(get) => {
            let text = to_text(get.units(), &ASSETS[get_i]);
            filled.give_text = give_text;
            filled.want_text = text.clone();
            drop(s);
            app.set_want_amount(text.into());
        }
        None => {
            filled.give_text = give_text;
            drop(s);
            app.set_offers_status("That amount is outside this offer's limits.".into());
        }
    }
}

/// Joins the offers topic of the selected network and keeps the list
/// current; quieter while another page is open.
async fn listen(weak: Weak<AppWindow>, state: Shared) {
    loop {
        let Some(app) = weak.upgrade() else { return };
        let page = app.get_page();
        let network_id = network(&app);
        drop(app);
        // The solver page shows the solver ID; ask for it once it is open.
        if page == 2 && state.borrow().did.is_empty() {
            load_identity(weak.clone(), state.clone()).await;
        }
        if let Some(network_id) = network_id {
            let (net, topic) = net_of(&network_id);
            if let Err(message) = hear(&state, net, topic).await {
                if let Some(app) = weak.upgrade() {
                    app.set_offers_status(format!("Offers: {message}").into());
                }
            } else if let Some(app) = weak.upgrade() {
                show(&app, &state);
            }
        }
        runtime::sleep_ms(if page == 1 { 15_000 } else { 4_000 }).await;
    }
}

async fn hear(state: &Shared, net: Net, topic: &str) -> Result<(), String> {
    let token = state.borrow().token.clone().unwrap_or_default();
    let joined = state.borrow().joined.clone();
    if joined.as_ref().map(|(t, _)| t.as_str()) != Some(topic) {
        // Another network: leave its topic first.
        if let Some((old, _)) = joined {
            let _ = runtime::peer("gossip_leave", &json!({ "topic": old }), &token).await;
        }
        join(topic, &token).await?;
        let mut s = state.borrow_mut();
        s.joined = Some((topic.to_string(), net));
        s.board = Board::default();
    }
    for _ in 0..RECV_CALLS {
        let reply = runtime::peer("gossip_recv", &json!({ "topic": topic, "limit": RECV_BATCH }), &token)
            .await
            .map_err(|e| e.message)?;
        let messages = reply["messages"].as_array().cloned().unwrap_or_default();
        let now = runtime::now_secs();
        let mut s = state.borrow_mut();
        for m in &messages {
            if let Some(content) = m["content"].as_str() {
                // Anything that does not verify is dropped unread.
                let _ = s.board.ingest(content, now, net);
            }
        }
        s.board.prune(now);
        if messages.len() < RECV_BATCH {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn join(topic: &str, token: &str) -> Result<(), String> {
    match runtime::peer("gossip_join", &json!({ "topic": topic }), token).await {
        Ok(_) => Ok(()),
        Err(e) if e.code == "already_joined" || e.message.contains("already") => Ok(()),
        Err(e) => Err(e.message),
    }
}

/// Marks a solver this Home finished a swap with: its offers come first and stay listed.
pub fn trust(solver: &str) {
    STATE.with(|s| {
        if let Some(state) = s.borrow().as_ref() {
            state.borrow_mut().board.trust(solver);
        }
    });
}

/// The solver of the offer the form was filled from, while the form still shows exactly
/// that offer's pair and amounts: the swap goes to that solver.
pub fn taken_offer(app: &AppWindow) -> Option<String> {
    STATE.with(|s| {
        let state = s.borrow().clone()?;
        let s = state.borrow();
        let filled = s.filled.as_ref()?;
        let o = &filled.listed.offer;
        let same = crate::intent::row_of(&o.want) == Some(app.get_give_index() as usize)
            && crate::intent::row_of(&o.give) == Some(app.get_want_index() as usize)
            && app.get_give_amount().trim() == filled.give_text
            && app.get_want_amount().trim() == filled.want_text
            && s.board.get(&filled.listed.solver, &o.id).is_some();
        same.then(|| filled.listed.solver.clone())
    })
}

/// The rows for the form's pair.
fn show(app: &AppWindow, state: &Shared) {
    let symbol = |i: i32| ASSETS.get(i as usize).map_or("", |a| a.symbol);
    let (give, want) = (symbol(app.get_give_index()), symbol(app.get_want_index()));
    let now = runtime::now_secs();
    let mut s = state.borrow_mut();
    let shown: Vec<offers::Listed> = match (offers::asset_at(app.get_give_index() as usize), offers::asset_at(app.get_want_index() as usize)) {
        (Some(g), Some(w)) => s.board.matching(&g, &w, now).into_iter().cloned().collect(),
        _ => vec![],
    };
    let rows: Vec<AddressRow> = shown
        .iter()
        .map(|l| {
            let (title, detail) = offers::describe(l, now);
            // A solver this Home finished swaps with stands out (bold) and says so.
            let trusted = s.board.is_trusted(&l.solver);
            let detail = if trusted { format!("Traded with before · {detail}") } else { detail };
            AddressRow { title: title.into(), detail: detail.into(), current: trusted }
        })
        .collect();
    s.shown = shown;
    // A new pair follows the market again.
    let pair = (app.get_give_index(), app.get_want_index());
    if pair != s.pair {
        s.pair = pair;
        s.manual = false;
    }
    // Follow the best offer: none taken yet, or a better one than the one followed.
    let best = s.shown.first().map(|l| (l.solver.clone(), l.offer.id.clone()));
    let followed = s.filled.as_ref().map(|f| (f.listed.solver.clone(), f.listed.offer.id.clone()));
    // Never while the review sheet is open or a swap runs: what is sent must be what was shown.
    let busy = app.get_review_open() || (app.get_swap_on() && !app.get_swap_finished());
    let auto_take = !s.manual && !busy && best.is_some() && best != followed;
    let following = s.filled.is_some();
    app.set_offers_pair(format!("{give} → {want}").into());
    if s.joined.is_some() && !following {
        app.set_offers_status(if rows.is_empty() {
            format!("No open offers for {give} → {want} right now.").into()
        } else {
            "Tap one to fill the form from it.".into()
        });
    }
    app.set_offer_rows(ModelRc::new(VecModel::from(rows)));
    drop(s);
    if auto_take {
        take(app, state, 0, true);
    }
}

/// Fills the swap form from an offer: the user's side and the most the rate
/// gives for the amount entered (or the offer's least, if none is entered or, when tapped by
/// hand, it is outside). Filled `auto`matically, it never changes an amount the user typed.
fn take(app: &AppWindow, state: &Shared, row: usize, auto: bool) -> bool {
    let mut s = state.borrow_mut();
    let Some(seen) = s.shown.get(row).cloned() else { return false };
    // Exactly the offer the row showed, and only while it is still listed so.
    let still = s.board.get(&seen.solver, &seen.offer.id).is_some_and(|now| Offer { expires_at: seen.offer.expires_at, ..now.offer.clone() } == seen.offer);
    if !still {
        drop(s);
        app.set_offers_status("That offer has ended.".into());
        return false;
    }
    let o = &seen.offer;
    let (Some(pay_i), Some(get_i)) = (crate::intent::row_of(&o.want), crate::intent::row_of(&o.give)) else { return false };
    let (pay_def, get_def) = (&ASSETS[pay_i], &ASSETS[get_i]);
    let typed = Amount::parse_decimal(&app.get_give_amount(), pay_def.decimals, pay_def.unit).ok();
    // What the user pays, on the form's own steps (rounded up to one).
    let least = o.price_of(o.min_give).map(|p| Amount::new(p.units().div_ceil(pay_def.unit) * pay_def.unit));
    let typed = typed.filter(|p| p.units() > 0);
    if auto && typed.is_some_and(|p| o.amount_for(p).is_none()) {
        drop(s);
        app.set_offers_status("Your amount is outside the best offer's limits: tap the offer to use its least amount.".into());
        return false;
    }
    let pay = typed.filter(|p| o.amount_for(*p).is_some()).or(least);
    let Some((pay, get)) = pay.and_then(|p| Some((p, o.amount_for(p)?))) else {
        drop(s);
        app.set_offers_status("This offer's limits are too narrow to fill from here.".into());
        return false;
    };
    let solver = offers::short_did(&seen.solver);
    let (give_text, want_text) = (to_text(pay.units(), pay_def), to_text(get.units(), get_def));
    s.filled = Some(Filled { listed: seen, give_text: give_text.clone(), want_text: want_text.clone() });
    drop(s);
    app.set_give_index(pay_i as i32);
    app.set_want_index(get_i as i32);
    app.set_give_amount(give_text.into());
    app.set_want_amount(want_text.into());
    app.set_form_error(SharedString::new());
    app.set_offers_status(format!("Filled from solver {solver}'s offer, at its rate.").into());
    true
}

async fn load_identity(weak: Weak<AppWindow>, state: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let (Some(network), Some(token)) = (network(&app), state.borrow().token.clone()) else {
        app.set_solver_error("Open Neptune Swap from Home to trade as a solver.".into());
        return;
    };
    drop(app);
    match runtime::provider("solver_identity", &json!({ "network": network }), &token).await {
        Ok(reply) => {
            let did = reply["did"].as_str().unwrap_or_default().to_string();
            state.borrow_mut().did = did.clone();
            if let Some(app) = weak.upgrade() {
                app.set_solver_did(offers::short_did(&did).into());
                app.set_solver_error(SharedString::new());
            }
        }
        Err(e) => {
            if let Some(app) = weak.upgrade() {
                let message = if e.message.contains("no wallet") { "Create a wallet first: the solver ID comes from its seed.".to_string() } else { e.message };
                app.set_solver_error(message.into());
            }
        }
    }
}

/// Signs `offer` with the solver key and sends it on the topic.
async fn send(state: &Shared, network_id: &str, offer: &Offer) -> Result<(), String> {
    let token = state.borrow().token.clone().ok_or("Open Neptune Swap from Home to trade as a solver.")?;
    let signed = runtime::provider("offer_sign", &json!({ "network": network_id, "offer": offer }), &token).await.map_err(|e| e.message)?;
    let topic = signed["topic"].as_str().ok_or("no topic")?.to_string();
    let message = signed["message"].as_str().ok_or("no signature")?.to_string();
    join(&topic, &token).await?;
    runtime::peer("gossip_send", &json!({ "topic": topic, "message": message }), &token).await.map_err(|e| e.message)?;
    Ok(())
}

async fn publish(weak: Weak<AppWindow>, state: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let Some(network_id) = network(&app) else {
        app.set_solver_error("No Neptune network is set up on this Home yet.".into());
        return;
    };
    let (price, min, max) = (app.get_solver_price(), app.get_solver_min(), app.get_solver_max());
    let form = SolverForm {
        gives_npt: app.get_solver_side() == 0,
        other: OTHERS[(app.get_solver_other() as usize).min(OTHERS.len() - 1)],
        price: &price,
        min_npt: &min,
        max_npt: &max,
        ttl_secs: COPY_SECS,
    };
    if state.borrow().mine.len() >= MINE_MAX {
        return app.set_solver_error(format!("At most {MINE_MAX} offers at a time: withdraw one first.").into());
    }
    let offer = match offers::offer_from(&form, crate::intent::random_id(), runtime::now_secs()) {
        Ok(offer) => offer,
        Err(message) => return app.set_solver_error(message.into()),
    };
    app.set_solver_busy(true);
    app.set_solver_error(SharedString::new());
    drop(app);
    let sent = send(&state, &network_id, &offer).await;
    let Some(app) = weak.upgrade() else { return };
    app.set_solver_busy(false);
    match sent {
        Ok(()) => {
            state.borrow_mut().mine.push(offer);
            show_mine(&app, &state);
            app.set_solver_note("Published. It stays live while this page is open, republished every ten minutes.".into());
        }
        Err(message) => app.set_solver_error(message.into()),
    }
}

/// Keeps this Home's offers alive: a fresh copy every ten minutes.
async fn republish(weak: Weak<AppWindow>, state: Shared) {
    loop {
        runtime::sleep_ms(REPUBLISH_SECS * 1000).await;
        let Some(app) = weak.upgrade() else { return };
        let Some(network_id) = network(&app) else { continue };
        drop(app);
        let now = runtime::now_secs();
        let offers: Vec<Offer> = state.borrow().mine.clone();
        for mut offer in offers {
            offer.expires_at = now + COPY_SECS;
            if send(&state, &network_id, &offer).await.is_ok() {
                if let Some(o) = state.borrow_mut().mine.iter_mut().find(|o| o.id == offer.id) {
                    o.expires_at = offer.expires_at;
                }
            }
        }
    }
}

async fn withdraw(weak: Weak<AppWindow>, state: Shared, row: usize) {
    let Some(app) = weak.upgrade() else { return };
    let Some(network_id) = network(&app) else { return };
    drop(app);
    let Some(offer) = state.borrow().mine.get(row).cloned() else { return };
    let last = Offer { withdrawn: true, expires_at: runtime::now_secs() + 120, ..offer.clone() };
    let sent = send(&state, &network_id, &last).await;
    let Some(app) = weak.upgrade() else { return };
    match sent {
        Ok(()) => {
            state.borrow_mut().mine.retain(|o| o.id != offer.id);
            show_mine(&app, &state);
            app.set_solver_note("Withdrawn.".into());
        }
        Err(message) => app.set_solver_error(message.into()),
    }
}

fn show_mine(app: &AppWindow, state: &Shared) {
    let now = runtime::now_secs();
    let s = state.borrow();
    let rows: Vec<AddressRow> = s
        .mine
        .iter()
        .map(|o| {
            let (title, detail) = offers::describe(&offers::Listed { offer: o.clone(), solver: s.did.clone() }, now);
            AddressRow { title: title.into(), detail: detail.into(), current: true }
        })
        .collect();
    app.set_solver_mine(ModelRc::new(VecModel::from(rows)));
}

/// Development screenshots: the offer list (`?demo=offers`) and the solver
/// page (`?demo=solver`), with sample offers.
pub fn demo(app: &AppWindow, state: &str) {
    app.set_page(2);
    let now = runtime::now_secs();
    let sample = |gives_npt: bool, other: &'static str, price: &'static str, min: &'static str, max: &'static str, solver: &str| {
        let form = SolverForm { gives_npt, other, price, min_npt: min, max_npt: max, ttl_secs: 1500 };
        offers::Listed { offer: offers::offer_from(&form, crate::intent::random_id(), now).expect("sample offer"), solver: solver.into() }
    };
    let row = |l: &offers::Listed, current: bool| {
        let (title, detail) = offers::describe(l, now);
        AddressRow { title: title.into(), detail: detail.into(), current }
    };
    if state == "solver" {
        app.set_page(2);
        app.set_solver_did("did:key:z6Mk…Qp7v".into());
        app.set_solver_price("0.16".into());
        app.set_solver_min("50".into());
        app.set_solver_max("5000".into());
        let mine = [sample(true, "USDC", "0.16", "50", "5000", "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK")];
        app.set_solver_mine(ModelRc::new(VecModel::from(mine.iter().map(|l| row(l, true)).collect::<Vec<_>>())));
        app.set_solver_note("Published. It stays live while this page is open, republished every ten minutes.".into());
        return;
    }
    let listed = [
        sample(true, "USDC", "0.149", "10", "10000", "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK"),
        sample(true, "USDC", "0.152", "100", "2500", "did:key:z6MkjchhfUsD6mmvni8mCdXHw216Xrm9bQe2mBH1P5RDjVJG"),
        sample(true, "USDC", "0.158", "1", "500", "did:key:z6MknGc3ocHs3zdPiJbnaaqDi58NGb4pk1Sp9WxWufuXSdxf"),
    ];
    app.set_offers_on(true);
    app.set_offers_pair("USDC → NPT".into());
    app.set_offers_status("Tap one to fill the form from it.".into());
    app.set_offer_rows(ModelRc::new(VecModel::from(listed.iter().map(|l| row(l, false)).collect::<Vec<_>>())));
}

#[cfg(test)]
mod tests {
    #[test]
    fn units_read_back_as_form_text() {
        let npt = &crate::intent::ASSETS[0];
        assert_eq!(super::to_text(4 * 10u128.pow(30) * 1500, npt), "1500");
        assert_eq!(super::to_text(2_666_666_666_666_666_666_666_666_666_666_666, npt), "666.666666666666666666666666666666");
        let usdc = &crate::intent::ASSETS[2];
        assert_eq!(super::to_text(225_000_000, usdc), "225");
        assert_eq!(super::to_text(1_500_001, usdc), "1.500001");
    }
}
