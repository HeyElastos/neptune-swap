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
            app.set_wallet_phrase_pairs(ModelRc::new(VecModel::<WordRow>::default()));
            app.set_wallet_state("ready".into());
            spawn(sync(weak.clone(), page.clone()));
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_sync(move || spawn(sync(weak.clone(), page.clone())));
    }
    {
        let weak = app.as_weak();
        app.on_wallet_start_send(move || {
            let app = weak.unwrap();
            app.set_wallet_error(SharedString::new());
            app.set_wallet_notice(SharedString::new());
            app.set_wallet_state("send".into());
        });
    }
    {
        let weak = app.as_weak();
        app.on_wallet_cancel_send(move || {
            let app = weak.unwrap();
            app.set_wallet_error(SharedString::new());
            let back = if app.get_wallet_state() == "confirm" { "send" } else { "ready" };
            app.set_wallet_state(back.into());
        });
    }
    {
        let weak = app.as_weak();
        app.on_wallet_review_send(move || {
            let app = weak.unwrap();
            match check_payment(&app.get_wallet_send_to(), &app.get_wallet_send_amount(), &app.get_wallet_send_fee()) {
                Ok(summary) => {
                    app.set_wallet_error(SharedString::new());
                    app.set_wallet_send_summary(summary.into());
                    app.set_wallet_state("confirm".into());
                }
                Err(message) => app.set_wallet_error(message.into()),
            }
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_confirm_send(move || spawn(send(weak.clone(), page.clone())));
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
            app.set_wallet_phrase_pairs(ModelRc::new(VecModel::from(word_rows_of(&words, 2))));
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

async fn send(weak: Weak<AppWindow>, page: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let body = json!({
        "to": app.get_wallet_send_to().split_whitespace().collect::<String>(),
        "amount": app.get_wallet_send_amount().trim(),
        "fee": app.get_wallet_send_fee().trim(),
    });
    app.set_wallet_busy(true);
    app.set_wallet_error(SharedString::new());
    drop(app);
    match call(&weak, &page, "wallet_send", body).await {
        Ok(sent) => {
            if let Some(app) = weak.upgrade() {
                app.set_wallet_send_to(SharedString::new());
                app.set_wallet_send_amount(SharedString::new());
                app.set_wallet_state("ready".into());
                app.set_wallet_notice(
                    format!(
                        "Sent {} NPT. It counts once a block includes it; {} NPT change comes back to this wallet.",
                        sent["sent"].as_str().unwrap_or("?"),
                        sent["change"].as_str().unwrap_or("0"),
                    )
                    .into(),
                );
            }
            sync(weak, page).await;
        }
        Err(message) => fail(&weak, message),
    }
}

/// The review line for a payment, or what is wrong with the form.
pub(crate) fn check_payment(to: &str, amount: &str, fee: &str) -> Result<String, String> {
    let to: String = to.split_whitespace().collect();
    if to.is_empty() {
        return Err("Enter the address to pay.".into());
    }
    if !to.starts_with("nolga") {
        return Err("That is not a Neptune address.".into());
    }
    let positive = |text: &str| {
        let t = text.trim();
        !t.is_empty()
            && t.chars().all(|c| c.is_ascii_digit() || c == '.')
            && t.matches('.').count() <= 1
            && t.chars().any(|c| c.is_ascii_digit() && c != '0')
    };
    if !positive(amount) {
        return Err("Enter an amount above zero, like 1.5.".into());
    }
    if !(positive(fee) || fee.trim().chars().all(|c| c == '0' || c == '.')) || fee.trim().is_empty() {
        return Err("Enter a fee, like 0.05.".into());
    }
    Ok(format!("Pay {} NPT to {}, with a fee of {} NPT.", amount.trim(), shorten(&to), fee.trim()))
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

/// The words `per_row` to a row, numbered; unused cells stay empty.
pub(crate) fn word_rows_of(words: &[String], per_row: usize) -> Vec<WordRow> {
    words
        .chunks(per_row)
        .enumerate()
        .map(|(row, chunk)| {
            let cell = |i: usize| chunk.get(i).map_or(SharedString::new(), |w| format!("{}. {w}", row * per_row + i + 1).into());
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
        "wallet-send" => {
            app.set_wallet_state("send".into());
            app.set_wallet_send_to(format!("nolgar1{}", "q".repeat(400)).into());
            app.set_wallet_send_amount("10".into());
        }
        "wallet-confirm" => {
            app.set_wallet_state("confirm".into());
            let summary = check_payment(&format!("nolgar1{}", "q".repeat(400)), "10", "0.05").unwrap_or_default();
            app.set_wallet_send_summary(summary.into());
        }
        "wallet-phrase" => {
            let words: Vec<String> = "abandon ability able about above absent absorb abstract absurd abuse access accident account accuse achieve acid acoustic acquire"
                .split(' ')
                .map(str::to_string)
                .collect();
            app.set_wallet_phrase_pairs(ModelRc::new(VecModel::from(word_rows_of(&words, 2))));
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
        let rows = word_rows_of(&words, 3);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].a, "1. w1");
        assert_eq!(rows[5].c, "18. w18");
        let pairs = word_rows_of(&words, 2);
        assert_eq!(pairs.len(), 9);
        assert_eq!(pairs[8].b, "18. w18");
        assert_eq!(pairs[8].c, "");
    }

    #[test]
    fn payments_are_checked_before_review() {
        let to = format!("nolgar1{}", "q".repeat(200));
        let ok = check_payment(&to, "10", "0.05").unwrap();
        assert!(ok.starts_with("Pay 10 NPT to nolgar1") && ok.ends_with("with a fee of 0.05 NPT."));
        assert!(check_payment("", "1", "0.05").is_err());
        assert!(check_payment("bc1qxyz", "1", "0.05").is_err());
        assert!(check_payment(&to, "0", "0.05").is_err());
        assert!(check_payment(&to, "-1", "0.05").is_err());
        assert!(check_payment(&to, "1.2.3", "0.05").is_err());
        assert!(check_payment(&to, "1", "").is_err());
    }

    #[test]
    fn sync_labels_read_well() {
        assert_eq!(sync_label(Some(1_500), 62_981, false), "Syncing: block 1,500 of 62,981.");
        assert_eq!(sync_label(Some(62_981), 62_981, true), "Up to date at block 62,981.");
        assert_eq!(sync_label(None, 10, false), "Syncing: the start of 10.");
    }
}
