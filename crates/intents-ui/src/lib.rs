//! Neptune Swap, the capsule's UI: Slint for the screen, Rust for everything behind it. The
//! form builds a protocol intent with `intents-protocol` itself, so the page and the solvers read
//! the same rules.

mod backdrop;
mod intent;
mod offers;
mod offers_ui;
mod qr;
mod runtime;
mod usd;
mod wallet_ui;

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

slint::include_modules!();

const NODE_MESSAGES: [(&str, &str); 3] = [
    ("unknown_network", "No Neptune node is set up for this network on this Home."),
    ("node_unreachable", "The local Neptune node is not answering. Is it running?"),
    ("node_error", "The local Neptune node sent an unexpected reply."),
];

pub fn run() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;

    let dark = backdrop::render(false);
    let light = backdrop::render(true);
    let frost = app.global::<Frost>();
    frost.set_wallpaper_dark(dark.wallpaper);
    frost.set_frost_dark(dark.frost);
    frost.set_wallpaper_light(light.wallpaper);
    frost.set_frost_light(light.frost);

    let assets: Vec<AssetView> = intent::ASSETS
        .iter()
        .map(|a| AssetView {
            letter: a.letter.into(),
            symbol: a.symbol.into(),
            network: a.network.into(),
            npt: a.symbol == "NPT",
            offered: a.chain.is_some(),
        })
        .collect();
    app.set_assets(ModelRc::new(VecModel::from(assets)));
    app.set_give_index(intent::DEFAULT_GIVE as i32);
    app.set_want_index(intent::DEFAULT_WANT as i32);

    let token = runtime::home_token();
    let prices: Rc<RefCell<Option<Value>>> = Rc::new(RefCell::new(None));
    {
        let (weak, prices) = (app.as_weak(), prices.clone());
        app.on_review(move || review(&weak.unwrap(), prices.borrow().as_ref()));
    }
    // The account elastOS Wallet disclosed for the EVM side, once connected.
    let account: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    {
        let (weak, prices, account) = (app.as_weak(), prices.clone(), account.clone());
        app.on_amounts_changed(move || {
            let app = weak.unwrap();
            show_usd(&app, prices.borrow().as_ref());
            show_other_leg(&app, account.borrow().as_deref());
            offers_ui::form_changed(&app);
        });
    }
    {
        let (weak, account, token) = (app.as_weak(), account.clone(), token.clone());
        app.on_connect_wallet(move || {
            let app = weak.unwrap();
            app.set_other_leg_state("asking".into());
            app.set_other_leg("Approve sharing your account in the Wallet app…".into());
            spawn(connect_wallet(weak.clone(), account.clone(), token.clone().unwrap_or_default()));
        });
    }
    show_other_leg(&app, None);
    {
        let (weak, prices, token) = (app.as_weak(), prices.clone(), token.clone());
        app.on_show_prices(move || {
            weak.unwrap().set_prices_state("asking".into());
            spawn(follow_prices(weak.clone(), prices.clone(), token.clone().unwrap_or_default()));
        });
    }
    let networks: Rc<VecModel<SharedString>> = Rc::new(VecModel::default());
    app.set_networks(ModelRc::from(networks.clone()));
    {
        let (weak, token) = (app.as_weak(), token.clone());
        app.on_pick_network(move |index| {
            weak.unwrap().set_network_index(index);
            spawn(load_status(weak.clone(), token.clone()));
        });
    }
    {
        let (weak, token) = (app.as_weak(), token.clone());
        app.on_refresh(move || spawn(load_status(weak.clone(), token.clone())));
    }

    wallet_ui::wire(&app, token.clone());
    offers_ui::wire(&app, token.clone());
    demo(&app);
    match token {
        Some(token) => {
            runtime::announce(&token);
            spawn(load_networks(app.as_weak(), Some(token), networks));
        }
        None => set_node(&app, "idle", "Not in Home", "Open Neptune Swap from Home to reach the Neptune node."),
    }
    // Development: NEPTUNE_SWAP_SIZE=360x760 lays the window out at that size (a phone).
    #[cfg(not(target_arch = "wasm32"))]
    if let Some((w, h)) = runtime::query("size").and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        app.window().set_size(slint::LogicalSize::new(w, h));
    }
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = runtime::query("shot") {
        shot_and_quit(&app, path);
    }
    app.run()
}

/// Development screenshots: once the window has drawn and its overlays settled, save it as a PNG
/// with the same renderer the capsule uses, and close.
#[cfg(not(target_arch = "wasm32"))]
fn shot_and_quit(app: &AppWindow, path: String) {
    let weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
        let app = weak.unwrap();
        let saved = app.window().take_snapshot().map_err(|e| e.to_string()).and_then(|shot| {
            let size = tiny_skia::IntSize::from_wh(shot.width(), shot.height()).ok_or("empty window")?;
            let pixmap = tiny_skia::Pixmap::from_vec(shot.as_bytes().to_vec(), size).ok_or("bad snapshot")?;
            pixmap.save_png(&path).map_err(|e| e.to_string())
        });
        match saved {
            Ok(()) => eprintln!("saved {path}"),
            Err(e) => eprintln!("screenshot failed: {e}"),
        }
        let _ = slint::quit_event_loop();
    });
}

/// The chain elastOS Wallet pays on for the EVM side: Base. Its app bridge
/// serves Base and Elastos ESC, not Ethereum main net.
const WALLET_CHAIN: (&str, &str) = ("eip155:8453", "Base");

/// What the form says about the side that is not NPT.
fn show_other_leg(app: &AppWindow, account: Option<&str>) {
    let symbol = |i: i32| intent::ASSETS.get(i as usize).map_or("", |a| a.symbol);
    let other = [symbol(app.get_give_index()), symbol(app.get_want_index())].into_iter().find(|s| *s != "NPT").unwrap_or("");
    let (state, text) = match (other, account) {
        ("ETH" | "USDC", Some(address)) => (
            "connected",
            format!(
                "{other} side: your elastOS Wallet account {} on {}. Wallet asks you to approve each payment, the way you set it up (passkey, MetaMask).",
                short_address(address),
                WALLET_CHAIN.1
            ),
        ),
        ("ETH" | "USDC", None) => ("connect", format!("Connect elastOS Wallet for the {other} side ({})", WALLET_CHAIN.1)),
        ("BTC", _) => ("info", "Bitcoin side: elastOS Wallet cannot send Bitcoin yet.".to_string()),
        _ => ("", String::new()),
    };
    if app.get_other_leg_state() != "asking" || state == "connected" {
        app.set_other_leg_state(state.into());
        app.set_other_leg(text.into());
    }
}

fn short_address(address: &str) -> String {
    if address.len() > 12 { format!("{}…{}", &address[..6], &address[address.len() - 4..]) } else { address.to_string() }
}

/// Asks elastOS Wallet for the account on `WALLET_CHAIN`; the user approves
/// in the Wallet app (once per 12 hours for this page).
async fn connect_wallet(weak: Weak<AppWindow>, account: Rc<RefCell<Option<String>>>, token: String) {
    let (page_url, origin) = runtime::page_and_origin();
    let fail = |weak: &Weak<AppWindow>, message: String| {
        if let Some(app) = weak.upgrade() {
            app.set_other_leg_state("connect".into());
            show_other_leg(&app, None);
            app.set_form_error(message.into());
        }
    };
    let asked = runtime::gateway(
        "POST",
        "/api/apps/browser/wallet/request-accounts",
        Some(&json!({ "chain_namespace": WALLET_CHAIN.0, "page_url": page_url, "origin": origin })),
        &token,
    )
    .await;
    let request_id = match asked {
        Ok(reply) => reply["approval_request"]["request_id"].as_str().unwrap_or_default().to_string(),
        Err(e) => return fail(&weak, format!("elastOS Wallet: {}", e.message)),
    };
    if request_id.is_empty() {
        return fail(&weak, "elastOS Wallet did not open an approval.".into());
    }
    let query = format!(
        "/api/apps/browser/wallet/approvals/{request_id}?page_url={}&origin={}",
        js_escape(&page_url),
        js_escape(&origin)
    );
    for _ in 0..300 {
        runtime::sleep_ms(2_000).await;
        match runtime::gateway("GET", &query, None, &token).await {
            Ok(status) if status["status"] == "completed" => {
                let address = status["account_access"]["address"].as_str().unwrap_or_default().to_string();
                if address.is_empty() {
                    return fail(&weak, "elastOS Wallet approved without an account.".into());
                }
                *account.borrow_mut() = Some(address);
                if let Some(app) = weak.upgrade() {
                    app.set_other_leg_state("connected".into());
                    show_other_leg(&app, account.borrow().as_deref());
                }
                return;
            }
            Ok(status) if matches!(status["status"].as_str(), Some("pending" | "approved")) => continue,
            Ok(status) => {
                return fail(&weak, format!("elastOS Wallet: the request was {}.", status["status"].as_str().unwrap_or("not approved")))
            }
            Err(e) => return fail(&weak, format!("elastOS Wallet: {}", e.message)),
        }
    }
    fail(&weak, "elastOS Wallet: no answer within ten minutes.".into());
}

/// Percent-encodes a query value.
fn js_escape(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The USD lines for the form as it stands.
fn show_usd(app: &AppWindow, prices: Option<&Value>) {
    let Some(prices) = prices else { return };
    let symbol = |i: i32| intent::ASSETS.get(i as usize).map_or("", |a| a.symbol);
    let lines = usd::lines(
        prices,
        (symbol(app.get_give_index()), &app.get_give_amount()),
        (symbol(app.get_want_index()), &app.get_want_amount()),
        runtime::now_secs(),
    );
    app.set_give_usd(lines.give.into());
    app.set_want_usd(lines.want.into());
    app.set_price_note(lines.note.into());
}

/// Asks the provider for prices (Home asks the user the first time), then
/// keeps them a minute fresh while the page is open.
async fn follow_prices(weak: Weak<AppWindow>, prices: Rc<RefCell<Option<Value>>>, token: String) {
    let mut waited = 0;
    loop {
        match runtime::provider("prices", &json!({}), &token).await {
            Ok(data) => {
                *prices.borrow_mut() = Some(data);
                let Some(app) = weak.upgrade() else { return };
                app.set_prices_state("on".into());
                show_usd(&app, prices.borrow().as_ref());
                drop(app);
                runtime::sleep_ms(60_000).await;
            }
            Err(e) if e.code == "approval_pending" && waited < 600 => {
                waited += 1;
                runtime::sleep_ms(2_000).await;
            }
            Err(e) => {
                let Some(app) = weak.upgrade() else { return };
                if prices.borrow().is_some() {
                    // Keep the last prices; try again in a minute.
                    drop(app);
                    runtime::sleep_ms(60_000).await;
                    continue;
                }
                app.set_prices_state(if e.code == "approval_denied" || e.code == "approval_pending" { "denied" } else { "off" }.into());
                if e.code != "approval_denied" && e.code != "approval_pending" {
                    app.set_form_error(e.message.into());
                }
                return;
            }
        }
    }
}

fn review(app: &AppWindow, prices: Option<&Value>) {
    let form = intent::Form {
        give: app.get_give_index() as usize,
        give_amount: &app.get_give_amount(),
        want: app.get_want_index() as usize,
        want_amount: &app.get_want_amount(),
        ttl: app.get_ttl_index() as usize,
    };
    match intent::build(&form, intent::random_id(), runtime::now_secs()) {
        Ok(built) => {
            let side = |index: usize, amount: &str| {
                let asset = &intent::ASSETS[index];
                let worth = prices
                    .and_then(|p| usd::price(p, asset.symbol))
                    .and_then(|p| usd::value(amount, p))
                    .map(|v| format!(" ({})", usd::format(v)))
                    .unwrap_or_default();
                format!("{} {}{worth} · {}", amount.trim(), asset.symbol, asset.network)
            };
            app.set_sum_give(side(form.give, form.give_amount).into());
            app.set_sum_want(side(form.want, form.want_amount).into());
            app.set_sum_until(intent::TTL.get(form.ttl).map_or("10 minutes", |t| t.1).into());
            app.set_review_json(intent::pretty(&built).into());
            app.set_form_error(SharedString::new());
            app.set_review_open(true);
        }
        Err(message) => app.set_form_error(message.into()),
    }
}

fn set_node(app: &AppWindow, state: &str, label: &str, detail: &str) {
    app.set_node_state(state.into());
    app.set_node_label(label.into());
    app.set_node_detail(detail.into());
}

async fn load_networks(weak: Weak<AppWindow>, token: Option<String>, networks: Rc<VecModel<SharedString>>) {
    let token_text = token.clone().unwrap_or_default();
    match runtime::provider("networks", &json!({}), &token_text).await {
        Ok(data) => {
            let ids: Vec<SharedString> = data["networks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n["id"].as_str().map(SharedString::from))
                .collect();
            networks.set_vec(ids);
            let Some(app) = weak.upgrade() else { return };
            if networks.row_count() == 0 {
                set_node(&app, "warn", "No node", "No Neptune network is set up on this Home yet.");
                return;
            }
            app.set_network_index(0);
            load_status(weak, token).await;
        }
        Err(e) => {
            if let Some(app) = weak.upgrade() {
                set_node(&app, "error", "Node", &e.message);
            }
        }
    }
}

async fn load_status(weak: Weak<AppWindow>, token: Option<String>) {
    let Some(app) = weak.upgrade() else { return };
    let index = app.get_network_index() as usize;
    let Some(network) = app.get_networks().row_data(index) else { return };
    set_node(&app, "idle", "Checking…", "");
    drop(app);
    let reply = runtime::provider("status", &json!({ "network": network.as_str() }), &token.unwrap_or_default()).await;
    let Some(app) = weak.upgrade() else { return };
    match reply {
        Ok(data) => {
            let height = data["height"].as_u64().unwrap_or_default();
            let label = format!("Block {}", group_thousands(height));
            set_node(&app, "ok", &label, &format!("Your node on {network}, outbound only."));
        }
        Err(e) => {
            let detail = NODE_MESSAGES.iter().find(|(code, _)| *code == e.code).map_or(e.message.as_str(), |m| m.1);
            set_node(&app, "error", "Node offline", detail);
        }
    }
}

pub(crate) fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Development only: `?demo=asset|node|review` (or NEPTUNE_SWAP_DEMO natively) opens that
/// state with a filled form, and `look=dark|light` pins the look, for screenshots.
fn demo(app: &AppWindow) {
    match runtime::query("look").as_deref() {
        Some("dark") => app.global::<Look>().set_pin(1),
        Some("light") => app.global::<Look>().set_pin(2),
        _ => {}
    }
    let Some(state) = runtime::query("demo") else { return };
    if state.starts_with("wallet") {
        wallet_ui::demo(app, &state);
        return;
    }
    app.set_give_amount("250".into());
    app.set_want_amount("1.5".into());
    if state == "offers" || state == "solver" {
        offers_ui::demo(app, &state);
        return;
    }
    if state == "prices" || state == "review-prices" {
        // Reference prices as the provider gives them (no network here).
        let prices = json!({"prices": {"NPT": {"usd": "0.1499"}, "USDC": {"usd": "0.9997"}, "BTC": {"usd": "64505.9"}, "ETH": {"usd": "1819.88"}}});
        app.set_want_amount("1600".into());
        app.set_prices_state("on".into());
        show_usd(app, Some(&prices));
        if state == "review-prices" {
            review(app, Some(&prices));
        }
        return;
    }
    if state == "review" {
        review(app, None);
        return;
    }
    // A popover anchors to its trigger, so it opens once the window has laid out.
    let weak = app.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
        let name = if state == "asset" { "want" } else { state.as_str() };
        weak.unwrap().invoke_demo_open(name.into());
    });
}

fn spawn(future: impl std::future::Future<Output = ()> + 'static) {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(future);
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Natively there is no Runtime to wait on: the stub answers at once.
        let _ = slint::spawn_local(future);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn thousands_are_grouped() {
        assert_eq!(super::group_thousands(0), "0");
        assert_eq!(super::group_thousands(999), "999");
        assert_eq!(super::group_thousands(21_337), "21,337");
        assert_eq!(super::group_thousands(1_234_567), "1,234,567");
    }
}
