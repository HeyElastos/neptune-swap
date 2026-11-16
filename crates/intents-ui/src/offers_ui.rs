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

#[derive(Default)]
struct State {
    token: Option<String>,
    board: Board,
    /// The topic joined, and the network it belongs to.
    joined: Option<(String, Net)>,
    /// The rows shown: (solver, offer id) in order.
    shown: Vec<(String, String)>,
    /// This Home's live offers.
    mine: Vec<Offer>,
    did: String,
}

type Shared = Rc<RefCell<State>>;

fn net_of(network_id: &str) -> (Net, &'static str) {
    if network_id == "neptune-main" {
        (Net::Main, intents_protocol::OFFERS_TOPIC_MAIN)
    } else {
        (Net::Test, intents_protocol::OFFERS_TOPIC_TEST)
    }
}

fn def_index(symbol: &str) -> Option<usize> {
    ASSETS.iter().position(|a| a.symbol == symbol)
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
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_take_offer(move |row| take(&weak.unwrap(), &state, row as usize));
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_publish_offer(move || spawn(publish(weak.clone(), state.clone())));
    }
    {
        let (weak, state) = (app.as_weak(), state.clone());
        app.on_withdraw_offer(move |row| spawn(withdraw(weak.clone(), state.clone(), row as usize)));
    }
    if token.is_some() {
        app.set_offers_status("Listening for offers on the carrier…".into());
        spawn(listen(app.as_weak(), state.clone()));
        spawn(republish(app.as_weak(), state));
    } else {
        app.set_offers_status("Open Neptune Swap from Home to see offers.".into());
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
    let joined = state.borrow().joined.as_ref().is_some_and(|(t, _)| t == topic);
    if !joined {
        match runtime::peer("gossip_join", &json!({ "topic": topic }), &token).await {
            Ok(_) => {}
            Err(e) if e.code == "already_joined" || e.message.contains("already") => {}
            Err(e) => return Err(e.message),
        }
        let mut s = state.borrow_mut();
        s.joined = Some((topic.to_string(), net));
        s.board = Board::default();
    }
    let reply = runtime::peer("gossip_recv", &json!({ "topic": topic, "limit": 50 }), &token).await.map_err(|e| e.message)?;
    let now = runtime::now_secs();
    let mut s = state.borrow_mut();
    for m in reply["messages"].as_array().into_iter().flatten() {
        if let Some(content) = m["content"].as_str() {
            // Anything that does not verify is dropped unread.
            let _ = s.board.ingest(content, now, net);
        }
    }
    s.board.prune(now);
    Ok(())
}

/// The rows for the form's pair.
fn show(app: &AppWindow, state: &Shared) {
    let symbol = |i: i32| ASSETS.get(i as usize).map_or("", |a| a.symbol);
    let (give, want) = (symbol(app.get_give_index()), symbol(app.get_want_index()));
    let now = runtime::now_secs();
    let mut s = state.borrow_mut();
    let listed = s.board.matching(give, want, now);
    let rows: Vec<AddressRow> = listed
        .iter()
        .map(|l| {
            let (title, detail) = offers::describe(l, now);
            AddressRow { title: title.into(), detail: detail.into(), current: false }
        })
        .collect();
    let shown = listed.iter().map(|l| (l.solver.clone(), l.offer.id.clone())).collect();
    drop(listed);
    s.shown = shown;
    app.set_offers_pair(format!("{give} → {want}").into());
    if s.joined.is_some() {
        app.set_offers_status(if rows.is_empty() {
            format!("No open offers for {give} → {want} right now.").into()
        } else {
            "Tap one to fill the form from it.".into()
        });
    }
    app.set_offer_rows(ModelRc::new(VecModel::from(rows)));
}

/// Fills the swap form from an offer: the user's side and the most the rate
/// gives for the amount entered (or the offer's least, if that is outside).
fn take(app: &AppWindow, state: &Shared, row: usize) {
    let s = state.borrow();
    let Some((solver, id)) = s.shown.get(row).cloned() else { return };
    let symbol = |i: i32| ASSETS.get(i as usize).map_or("", |a| a.symbol);
    let (give, want) = (symbol(app.get_give_index()), symbol(app.get_want_index()));
    let Some(l) = s.board.matching(give, want, runtime::now_secs()).into_iter().find(|l| l.solver == solver && l.offer.id == id)
    else {
        return;
    };
    let (Some(pay_i), Some(get_i)) = (def_index(&l.offer.want.symbol), def_index(&l.offer.give.symbol)) else { return };
    let (pay_def, get_def) = (&ASSETS[pay_i], &ASSETS[get_i]);
    let typed = Amount::parse_decimal(&app.get_give_amount(), pay_def.decimals, pay_def.unit).ok();
    let pay = typed.filter(|p| l.offer.amount_for(*p).is_some()).or_else(|| l.offer.price_of(l.offer.min_give));
    let Some(pay) = pay else { return };
    let Some(get) = l.offer.amount_for(pay) else { return };
    let solver = offers::short_did(&l.solver);
    drop(s);
    app.set_give_index(pay_i as i32);
    app.set_want_index(get_i as i32);
    app.set_give_amount(to_text(pay.units(), pay_def).into());
    app.set_want_amount(to_text(get.units(), get_def).into());
    app.set_form_error(SharedString::new());
    app.set_offers_status(format!("Filled from {solver}'s offer. Review it, then it goes sealed to that solver.").into());
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
    match runtime::peer("gossip_join", &json!({ "topic": topic }), &token).await {
        Ok(_) => {}
        Err(e) if e.code == "already_joined" || e.message.contains("already") => {}
        Err(e) => return Err(e.message),
    }
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
        other: OTHERS[(app.get_solver_other() as usize).min(2)],
        price: &price,
        min_npt: &min,
        max_npt: &max,
        ttl_secs: COPY_SECS,
    };
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
