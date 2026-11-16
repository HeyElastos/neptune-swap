//! Neptune Intents, the capsule's UI: Slint for the screen, Rust for everything behind it. The
//! form builds a protocol intent with `intents-protocol` itself, so the page and the solvers read
//! the same rules.

mod backdrop;
mod intent;
mod qr;
mod runtime;
mod wallet_ui;

use std::rc::Rc;

use serde_json::json;
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

    let weak = app.as_weak();
    app.on_review(move || review(&weak.unwrap()));

    let token = runtime::home_token();
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
    demo(&app);
    match token {
        Some(token) => {
            runtime::announce(&token);
            spawn(load_networks(app.as_weak(), Some(token), networks));
        }
        None => set_node(&app, "idle", "Not in Home", "Open Neptune Intents from Home to reach the Neptune node."),
    }
    // Development: NEPTUNE_INTENTS_SIZE=360x760 lays the window out at that size (a phone).
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

fn review(app: &AppWindow) {
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
                format!("{} {} · {}", amount.trim(), asset.symbol, asset.network)
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

/// Development only: `?demo=asset|node|review` (or NEPTUNE_INTENTS_DEMO natively) opens that
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
    if state == "review" {
        review(app);
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
