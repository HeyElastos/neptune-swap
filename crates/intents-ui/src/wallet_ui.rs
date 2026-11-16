//! The wallet page: every action is a typed call to this Home's Neptune
//! provider, which holds the seed. The page keeps only what it shows.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::{runtime, spawn, ActivityRow, AppWindow, WordRow};

/// Address kinds, as the receive switch numbers them and the provider names them.
const KINDS: [&str; 2] = ["generation", "ec_hybrid"];
/// Payments the overview lists; the rest are under All activity.
const OVERVIEW_ROWS: usize = 5;

/// What the page remembers between calls.
#[derive(Default)]
struct Page {
    token: Option<String>,
    /// The full current address of each kind, for copying; the card shows it shortened.
    addresses: [String; 2],
    /// Every payment, newest first.
    activity: Vec<ActivityRow>,
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
        let weak = app.as_weak();
        app.on_wallet_paste_address(move || {
            let current = weak.upgrade().map(|app| app.get_wallet_send_to().to_string()).unwrap_or_default();
            let weak = weak.clone();
            runtime::ask_text(
                "Address to pay",
                &current,
                "Paste the address (⌘V or Ctrl+V), then choose Use.",
                "Use",
                Box::new(move |text| {
                    if let Some(app) = weak.upgrade() {
                        app.set_wallet_send_to(text.split_whitespace().collect::<String>().into());
                    }
                }),
            );
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_start_receive(move || {
            let app = weak.unwrap();
            app.set_wallet_error(SharedString::new());
            app.set_wallet_copy_note(SharedString::new());
            app.set_wallet_state("receive".into());
            spawn(load_addresses(weak.clone(), page.clone()));
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_receive_kind_changed(move |_| {
            if let Some(app) = weak.upgrade() {
                app.set_wallet_copy_note(SharedString::new());
            }
            show_receive(&weak, &page);
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_new_address(move || spawn(new_address(weak.clone(), page.clone())));
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_show_history(move || {
            let app = weak.unwrap();
            app.set_wallet_activity(ModelRc::new(VecModel::from(page.borrow().activity.clone())));
            app.set_wallet_state("history".into());
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_close_view(move || {
            let app = weak.unwrap();
            app.set_wallet_copy_note(SharedString::new());
            show_activity(&app, &page.borrow().activity);
            app.set_wallet_state("ready".into());
        });
    }
    {
        let (weak, page) = on(&page);
        app.on_wallet_copy_address(move || {
            let (token, address, kind) = {
                let p = page.borrow();
                let kind = weak.upgrade().map_or(0, |app| app.get_wallet_receive_kind() as usize).min(1);
                (p.token.clone().unwrap_or_default(), p.addresses[kind].clone(), kind)
            };
            // The whole address, selected, in a plain HTML panel: ⌘C copies a
            // selection without any clipboard permission. Home's bridge is
            // tried as well, for one-click copying where Home allows it.
            let title = if kind == 0 { "Your Generation address" } else { "Your EC hybrid address" };
            let copied = runtime::show_text(title, &address);
            if let Some(app) = weak.upgrade() {
                app.set_wallet_copy_note(SharedString::new());
            }
            if !copied {
                let weak = weak.clone();
                runtime::copy_text(&token, "neptune.address", &address, Box::new(move |result| {
                    if let (Ok(()), Some(app)) = (result, weak.upgrade()) {
                        app.set_wallet_copy_note("Copied.".into());
                    }
                }));
            }
        });
    }
}

fn network(app: &AppWindow) -> Option<String> {
    app.get_networks().row_data(app.get_network_index() as usize).map(|n| n.to_string())
}

/// A provider call for the selected network, or the reason there is none.
async fn call(weak: &Weak<AppWindow>, page: &Shared, op: &str, mut body: Value) -> Result<Value, String> {
    let app = weak.upgrade().ok_or("closed")?;
    let token = page.borrow().token.clone().ok_or("Open Neptune Swap from Home to use the wallet.")?;
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
                    let verified = height.is_some() && step["verified_height"].as_u64() == height;
                    app.set_wallet_sync_label(sync_label(height, tip, step["done"] == true, verified).into());
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
            drop(app);
            load_history(&weak, &page).await;
        }
        Err(message) => fail(&weak, message),
    }
}

async fn load_history(weak: &Weak<AppWindow>, page: &Shared) {
    match call(weak, page, "wallet_history", json!({})).await {
        Ok(reply) => {
            let rows: Vec<ActivityRow> = reply["items"].as_array().into_iter().flatten().map(activity_row).collect();
            page.borrow_mut().activity = rows;
            if let Some(app) = weak.upgrade() {
                if app.get_wallet_state() == "history" {
                    app.set_wallet_activity(ModelRc::new(VecModel::from(page.borrow().activity.clone())));
                } else {
                    show_activity(&app, &page.borrow().activity);
                }
            }
        }
        Err(message) => fail(weak, message),
    }
}

fn show_activity(app: &AppWindow, rows: &[ActivityRow]) {
    let shown: Vec<ActivityRow> = rows.iter().take(OVERVIEW_ROWS).cloned().collect();
    app.set_wallet_activity(ModelRc::new(VecModel::from(shown)));
    app.set_wallet_activity_more(rows.len() > OVERVIEW_ROWS);
}

/// One provider history item as a row: direction, signed amount, what and when.
pub(crate) fn activity_row(item: &Value) -> ActivityRow {
    let incoming = item["direction"] == "in";
    let pending = item["pending"] == true;
    let amount = item["amount"].as_str().unwrap_or("0");
    let title = match (incoming, pending) {
        (true, _) => match item["kind"].as_str() {
            Some("ec_hybrid") => "Received at a short address",
            _ => "Received",
        },
        (false, true) => "Sending",
        (false, false) => "Sent",
    };
    let detail = if pending {
        "Waiting for a block".to_string()
    } else {
        let block = item["height"].as_u64().map(|h| format!("Block {}", crate::group_thousands(h)));
        let when = item["time_ms"].as_u64().map(runtime::format_time);
        [block, when].into_iter().flatten().collect::<Vec<_>>().join(" · ")
    };
    ActivityRow {
        incoming,
        amount: format!("{}{amount}", if incoming { "+" } else { "−" }).into(),
        title: title.into(),
        detail: detail.into(),
        pending,
    }
}

async fn load_addresses(weak: Weak<AppWindow>, page: Shared) {
    match call(&weak, &page, "wallet_addresses", json!({})).await {
        Ok(reply) => {
            {
                let mut p = page.borrow_mut();
                for (i, kind) in KINDS.iter().enumerate() {
                    if let Some(address) = reply[*kind]["address"].as_str() {
                        p.addresses[i] = address.to_string();
                    }
                }
            }
            show_receive(&weak, &page);
        }
        Err(message) => fail(&weak, message),
    }
}

async fn new_address(weak: Weak<AppWindow>, page: Shared) {
    let Some(app) = weak.upgrade() else { return };
    let kind = (app.get_wallet_receive_kind() as usize).min(1);
    app.set_wallet_busy(true);
    app.set_wallet_copy_note(SharedString::new());
    drop(app);
    match call(&weak, &page, "wallet_new_address", json!({ "kind": KINDS[kind] })).await {
        Ok(reply) => {
            page.borrow_mut().addresses[kind] = reply["address"].as_str().unwrap_or_default().to_string();
            show_receive(&weak, &page);
            if let Some(app) = weak.upgrade() {
                app.set_wallet_busy(false);
                app.set_wallet_copy_note("A new address. Earlier ones still receive.".into());
            }
        }
        Err(message) => fail(&weak, message),
    }
}

/// The receive view for the chosen kind: shortened address, and a QR code for a short one.
fn show_receive(weak: &Weak<AppWindow>, page: &Shared) {
    let Some(app) = weak.upgrade() else { return };
    let kind = (app.get_wallet_receive_kind() as usize).min(1);
    let address = page.borrow().addresses[kind].clone();
    app.set_wallet_address_short(shorten(&address).into());
    let qr = (!address.is_empty()).then(|| crate::qr::image(&address)).flatten();
    app.set_wallet_qr_shown(qr.is_some());
    if let Some(image) = qr {
        app.set_wallet_qr(image);
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
    app.set_wallet_notice("Preparing the payment…".into());
    drop(app);
    // The provider proves in the background (about half a minute per coin
    // spent on a real network) and answers with a job to ask about.
    let job = match call(&weak, &page, "wallet_send", body).await {
        Ok(started) => started["job"].as_str().unwrap_or_default().to_string(),
        Err(message) => return fail_send(&weak, message),
    };
    if let Some(app) = weak.upgrade() {
        app.set_wallet_notice("Proving the payment on this computer. This takes about half a minute for each coin it spends.".into());
    }
    let sent = loop {
        runtime::sleep_ms(2000).await;
        match call(&weak, &page, "wallet_send_status", json!({ "job": job })).await {
            Ok(status) if status["state"] == "proving" => continue,
            Ok(status) if status["state"] == "sent" => break status,
            Ok(status) => {
                return fail_send(&weak, status["message"].as_str().unwrap_or("The payment failed.").to_string())
            }
            Err(message) => return fail_send(&weak, message),
        }
    };
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

/// A payment that did not go out: back to the review, with the reason.
fn fail_send(weak: &Weak<AppWindow>, message: String) {
    if let Some(app) = weak.upgrade() {
        app.set_wallet_notice(SharedString::new());
    }
    fail(weak, message);
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
    page.borrow_mut().addresses[0] = address.to_string();
    if let Some(app) = weak.upgrade() {
        app.set_wallet_address_short(shorten(address).into());
        app.set_wallet_copy_note(SharedString::new());
    }
}

/// `verified`: this wallet checked the chain proof of the block it is at.
pub(crate) fn sync_label(height: Option<u64>, tip: u64, done: bool, verified: bool) -> String {
    let at = height.map_or("the start".to_string(), |h| format!("block {}", crate::group_thousands(h)));
    if done && verified {
        format!("Up to date at {at}; its chain proof checked here.")
    } else if done {
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
    slint::Timer::single_shot(std::time::Duration::from_millis(1000), move || demo_state(&weak.unwrap(), &state));
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
        "wallet-receive" | "wallet-receive-private" => {
            app.set_wallet_state("receive".into());
            app.set_wallet_receive_kind(if state == "wallet-receive" { 1 } else { 0 });
            // After the switch's own change handler, which shows the page's (empty) addresses.
            let (weak, state) = (app.as_weak(), state.to_string());
            slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
                let app = weak.unwrap();
                let generation = format!("nolgam1{}", "q".repeat(3000));
                let short = format!("nolgaz1{}", "qpzry9x8gf2tvdw0s3jn54khce6mua7l".repeat(4));
                app.set_wallet_address_short(shorten(if state == "wallet-receive" { &short } else { &generation }).into());
                let qr = (state == "wallet-receive").then(|| crate::qr::image(&short)).flatten();
                app.set_wallet_qr_shown(qr.is_some());
                if let Some(image) = qr {
                    app.set_wallet_qr(image);
                }
            });
        }
        "wallet-history" => {
            app.set_wallet_state("history".into());
            app.set_wallet_activity(ModelRc::new(VecModel::from(demo_activity())));
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
            app.set_wallet_sync_label(sync_label(Some(62_981), 62_981, true, true).into());
            app.set_wallet_address_short(shorten(&format!("nolgam1{}", "q".repeat(3000))).into());
            let rows = demo_activity();
            show_activity(app, &rows);
        }
    }
}

fn demo_activity() -> Vec<ActivityRow> {
    let items = [
        json!({ "direction": "out", "amount": "10.05", "pending": true }),
        json!({ "direction": "in", "amount": "25", "height": 212, "time_ms": 1_794_825_766_000u64, "kind": "generation" }),
        json!({ "direction": "out", "amount": "3.2", "height": 198, "time_ms": 1_794_825_100_000u64 }),
        json!({ "direction": "in", "amount": "0.75", "height": 150, "time_ms": 1_794_824_000_000u64, "kind": "ec_hybrid" }),
        json!({ "direction": "in", "amount": "120", "height": 97, "time_ms": 1_794_820_000_000u64, "kind": "generation" }),
        json!({ "direction": "out", "amount": "40.5", "height": 61, "time_ms": 1_794_810_000_000u64 }),
        json!({ "direction": "in", "amount": "1", "height": 12, "time_ms": 1_794_800_000_000u64, "kind": "generation" }),
    ];
    items.iter().map(activity_row).collect()
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
    fn history_items_read_as_rows() {
        let received = activity_row(&json!({ "direction": "in", "amount": "25", "height": 1212, "time_ms": 0, "kind": "generation" }));
        assert!(received.incoming && !received.pending);
        assert_eq!(received.amount, "+25");
        assert_eq!(received.title, "Received");
        assert_eq!(received.detail, "Block 1,212 · 1970-01-01 00:00 UTC");
        let sending = activity_row(&json!({ "direction": "out", "amount": "10.05", "pending": true }));
        assert_eq!((sending.amount.as_str(), sending.title.as_str(), sending.detail.as_str()), ("−10.05", "Sending", "Waiting for a block"));
        let short = activity_row(&json!({ "direction": "in", "amount": "1", "kind": "ec_hybrid" }));
        assert_eq!(short.title, "Received at a short address");
    }

    #[test]
    fn block_times_format_as_utc_natively() {
        assert_eq!(crate::runtime::format_time(1_794_825_766_000), "2026-11-16 10:42 UTC");
        assert_eq!(crate::runtime::format_time(951_782_400_000), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn sync_labels_read_well() {
        assert_eq!(sync_label(Some(1_500), 62_981, false, false), "Syncing: block 1,500 of 62,981.");
        assert_eq!(sync_label(Some(62_981), 62_981, true, false), "Up to date at block 62,981.");
        assert_eq!(sync_label(Some(62_981), 62_981, true, true), "Up to date at block 62,981; its chain proof checked here.");
        assert_eq!(sync_label(None, 10, false, false), "Syncing: the start of 10.");
    }
}
