//! The wallet page: every action is a typed call to this Home's Neptune
//! provider, which holds the seed. The page keeps only what it shows.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::{runtime, spawn, AppWindow, WordRow};

/// What the page remembers between calls.
#[derive(Default)]
struct Page {
    token: Option<String>,
    /// The full receiving address, for copying; the card shows it shortened.
    address: String,
}

type Shared = Rc<RefCell<Page>>;

pub fn wire(app: &AppWindow, token: Option<String>) {
    let page: Shared = Rc::new(RefCell::new(Page { token, ..Page::default() }));
    runtime::listen_clipboard();

    let on = |page: &Shared| (app.as_weak(), page.clone());
    {
        let (weak, page) = on(&page);
        app.on_page_changed(move |index| {
            if index == 1 {
                spawn(load(weak.clone(), page.clone()));
            }
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_create(move || spawn(create(weak.clone(), page.clone())));
    }
    {
        let weak = app.as_weak();
        app.on_wallet_start_import(move || {
            let app = weak.unwrap();
            app.set_wallet_error(SharedString::new());
            app.set_wallet_state("import".into());
        });
    }
    {
        let weak = app.as_weak();
        app.on_wallet_cancel_import(move || {
            let app = weak.unwrap();
            app.set_wallet_import_phrase(SharedString::new());
            app.set_wallet_error(SharedString::new());
            app.set_wallet_state("none".into());
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_import(move || spawn(import(weak.clone(), page.clone())));
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_phrase_done(move || {
            let app = weak.unwrap();
            // The words leave the page for good.
            app.set_wallet_phrase_rows(ModelRc::new(VecModel::<WordRow>::default()));
            app.set_wallet_state("ready".into());
            spawn(sync(weak.clone(), page.clone()));
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_sync(move || spawn(sync(weak.clone(), page.clone())));
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_copy_address(move || {
            let (token, address) = {
                let p = page.borrow();
                (p.token.clone().unwrap_or_default(), p.address.clone())
            };
            let weak = weak.clone();
            runtime::copy_text(&token, "neptune.address", &address, Box::new(move |result| {
                let Some(app) = weak.upgrade() else { return };
                app.set_wallet_copy_note(match result {
                    Ok(()) => "Copied.".into(),
                    Err(_) => "This Home did not let the app copy. Select the address instead.".into(),
                });
            }));
        });
    }
}

fn network(app: &AppWindow) -> Option<String> {
    app.get_networks().row_data(app.get_network_index() as usize).map(|n| n.to_string())
}

/// A provider call for the selected network, or the reason there is none.
async fn call(weak: &Weak<AppWindow>, page: &Shared, op: &str, mut body: Value) -> Result<Value, String> {
    let app = weak.upgrade().ok_or("closed")?;
    let token = page.borrow().token.clone().ok_or("Open Neptune Intents from Home to use the wallet.")?;
    let network = network(&app).ok_or("No Neptune network is set up on this Home yet.")?;
    drop(app);
    body["network"] = json!(network);
    runtime::provider(op, &body, &token).await.map_err(|e| e.message)
}

fn fail(weak: &Weak<AppWindow>, message: String) {
    if let Some(app) = weak.upgrade() {
        app.set_wallet_busy(false);
        app.set_wallet_error(message.into());
    }
}

async fn load(weak: Weak<AppWindow>, page: Shared) {
    if let Some(app) = weak.upgrade() {
        app.set_wallet_error(SharedString::new());
        if app.get_wallet_state() == "phrase" || app.get_wallet_state() == "import" {
            return;
        }
    }
    match call(&weak, &page, "wallet_status", json!({})).await {
        Ok(status) if status["wallet"] == true => {
            show_address(&weak, &page, status["address"].as_str().unwrap_or_default());
            if let Some(app) = weak.upgrade() {
                app.set_wallet_state("ready".into());
            }
            sync(weak, page).await;
        }
        Ok(_) => {
            if let Some(app) = weak.upgrade() {
                app.set_wallet_state("none".into());
            }
        }
        Err(message) => {
            if let Some(app) = weak.upgrade() {
                app.set_wallet_state("none".into());
            }
            fail(&weak, message);
        }
    }
}

async fn create(weak: Weak<AppWindow>, page: Shared) {
    if let Some(app) = weak.upgrade() {
        app.set_wallet_busy(true);
        app.set_wallet_error(SharedString::new());
    }
    match call(&weak, &page, "wallet_create", json!({})).await {
        Ok(created) => {
            show_address(&weak, &page, created["address"].as_str().unwrap_or_default());
            let words: Vec<String> = created["phrase"].as_str().unwrap_or_default().split(' ').map(str::to_string).collect();
            let Some(app) = weak.upgrade() else { return };
            app.set_wallet_phrase_rows(ModelRc::new(VecModel::from(word_rows(&words))));
            app.set_wallet_busy(false);
            app.set_wallet_state("phrase".into());
        }
        Err(message) => fail(&weak, message),
    }
}

async fn import(weak: Weak<AppWindow>, page: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let phrase = app.get_wallet_import_phrase().to_string();
    let height = app.get_wallet_import_height().trim().to_string();
    let from_height: u64 = if height.is_empty() {
        0
    } else {
        match height.parse() {
            Ok(h) => h,
            Err(_) => return fail(&weak, "The block height must be a whole number.".into()),
        }
    };
    if phrase.split_whitespace().count() != 18 {
        return fail(&weak, "A Neptune seed has 18 words.".into());
    }
    app.set_wallet_busy(true);
    app.set_wallet_error(SharedString::new());
    drop(app);
    match call(&weak, &page, "wallet_import", json!({ "phrase": phrase, "from_height": from_height })).await {
        Ok(imported) => {
            show_address(&weak, &page, imported["address"].as_str().unwrap_or_default());
            if let Some(app) = weak.upgrade() {
                app.set_wallet_import_phrase(SharedString::new());
                app.set_wallet_busy(false);
                app.set_wallet_state("ready".into());
            }
            sync(weak, page).await;
        }
        Err(message) => fail(&weak, message),
    }
}

/// Syncs until the wallet reaches the node's tip, then reads the balance.
async fn sync(weak: Weak<AppWindow>, page: Shared) {
    if let Some(app) = weak.upgrade() {
        app.set_wallet_busy(true);
        app.set_wallet_error(SharedString::new());
    }
    loop {
        match call(&weak, &page, "wallet_sync", json!({})).await {
            Ok(step) => {
                let (height, tip) = (step["synced_height"].as_u64(), step["tip"].as_u64().unwrap_or_default());
                if let Some(app) = weak.upgrade() {
                    app.set_wallet_sync_label(sync_label(height, tip, step["done"] == true).into());
                }
                if step["done"] == true {
                    break;
                }
            }
            Err(message) => return fail(&weak, message),
        }
    }
    match call(&weak, &page, "wallet_balance", json!({})).await {
        Ok(reply) => {
            let Some(app) = weak.upgrade() else { return };
            let total = reply["balance"]["total"].as_str().unwrap_or("0").to_string();
            let spendable = reply["balance"]["spendable"].as_str().unwrap_or("0").to_string();
            app.set_wallet_balance(total.clone().into());
            app.set_wallet_spendable(if spendable == total {
                SharedString::new()
            } else {
                format!("{spendable} NPT spendable now; the rest is time-locked.").into()
            });
            app.set_wallet_busy(false);
        }
        Err(message) => fail(&weak, message),
    }
}

fn show_address(weak: &Weak<AppWindow>, page: &Shared, address: &str) {
    page.borrow_mut().address = address.to_string();
    if let Some(app) = weak.upgrade() {
        app.set_wallet_address_short(shorten(address).into());
        app.set_wallet_copy_note(SharedString::new());
    }
}

pub(crate) fn sync_label(height: Option<u64>, tip: u64, done: bool) -> String {
    let at = height.map_or("the start".to_string(), |h| format!("block {}", crate::group_thousands(h)));
    if done {
        format!("Up to date at {at}.")
    } else {
        format!("Syncing: {at} of {}.", crate::group_thousands(tip))
    }
}

/// Generation addresses run to thousands of characters: show both ends.
pub(crate) fn shorten(address: &str) -> String {
    if address.len() <= 48 {
        return address.to_string();
    }
    format!("{}…{}", &address[..24], &address[address.len() - 16..])
}

pub(crate) fn word_rows(words: &[String]) -> Vec<WordRow> {
    words
        .chunks(3)
        .enumerate()
        .map(|(row, chunk)| {
            let cell = |i: usize| chunk.get(i).map_or(SharedString::new(), |w| format!("{}. {w}", row * 3 + i + 1).into());
            WordRow { a: cell(0), b: cell(1), c: cell(2) }
        })
        .collect()
}

/// Development screenshots of the wallet states (`?demo=wallet-…`).
pub fn demo(app: &AppWindow, state: &str) {
    app.set_page(1);
    // After the page change's own load has run (outside Home it finds no wallet).
    let (weak, state) = (app.as_weak(), state.to_string());
    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || demo_state(&weak.unwrap(), &state));
}

fn demo_state(app: &AppWindow, state: &str) {
    app.set_wallet_error(SharedString::new());
    match state {
        "wallet-none" => app.set_wallet_state("none".into()),
        "wallet-import" => {
            app.set_wallet_state("import".into());
            app.set_wallet_import_height("62000".into());
        }
        "wallet-phrase" => {
            let words: Vec<String> = "abandon ability able about above absent absorb abstract absurd abuse access accident account accuse achieve acid acoustic acquire"
                .split(' ')
                .map(str::to_string)
                .collect();
            app.set_wallet_phrase_rows(ModelRc::new(VecModel::from(word_rows(&words))));
            app.set_wallet_state("phrase".into());
        }
        _ => {
            app.set_wallet_state("ready".into());
            app.set_wallet_balance("12.5".into());
            app.set_wallet_spendable("10 NPT spendable now; the rest is time-locked.".into());
            app.set_wallet_sync_label(sync_label(Some(62_981), 62_981, true).into());
            app.set_wallet_address_short(shorten(&format!("nolgam1{}", "q".repeat(3000))).into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_addresses_keep_both_ends() {
        let address = format!("nolgam1{}xyz", "q".repeat(3000));
        let short = shorten(&address);
        assert!(short.starts_with("nolgam1") && short.ends_with("qqqqqqqqqqqqqxyz") && short.contains('…'));
        assert_eq!(shorten("short"), "short");
    }

    #[test]
    fn eighteen_words_make_six_numbered_rows() {
        let words: Vec<String> = (1..=18).map(|i| format!("w{i}")).collect();
        let rows = word_rows(&words);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].a, "1. w1");
        assert_eq!(rows[5].c, "18. w18");
    }

    #[test]
    fn sync_labels_read_well() {
        assert_eq!(sync_label(Some(1_500), 62_981, false), "Syncing: block 1,500 of 62,981.");
        assert_eq!(sync_label(Some(62_981), 62_981, true), "Up to date at block 62,981.");
        assert_eq!(sync_label(None, 10, false), "Syncing: the start of 10.");
    }
}
