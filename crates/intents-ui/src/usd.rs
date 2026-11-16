//! What the swap form's amounts are worth in USD, from the provider's
//! reference prices. Display only: the intent carries exact token amounts and
//! nothing here changes them.

use serde_json::Value;

/// The USD price of `symbol` in a `prices` reply, if a source priced it.
pub fn price(prices: &Value, symbol: &str) -> Option<f64> {
    prices["prices"][symbol]["usd"].as_str()?.parse::<f64>().ok().filter(|p| p.is_finite() && *p > 0.0)
}

/// `amount` (as typed) of an asset priced at `price`, in USD.
pub fn value(amount: &str, price: f64) -> Option<f64> {
    let amount = amount.trim().parse::<f64>().ok().filter(|a| a.is_finite() && *a > 0.0)?;
    Some(amount * price)
}

/// "≈ $1,234.56", "≈ $0.42", "< $0.01".
pub fn format(usd: f64) -> String {
    if usd < 0.01 {
        return "< $0.01".into();
    }
    let cents = (usd * 100.0).round() as u128;
    format!("≈ ${}.{:02}", group(cents / 100), cents % 100)
}

fn group(n: u128) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 NPT ≈ $0.15": a unit price, with enough digits for a cheap coin.
pub fn unit(symbol: &str, price: f64) -> String {
    let text = if price >= 100.0 {
        format!("${}", group(price.round() as u128))
    } else if price >= 1.0 {
        format!("${price:.2}")
    } else {
        format!("${price:.4}")
    };
    format!("1 {symbol} ≈ {text}")
}

/// How the asked amount compares with the market: what you want is worth
/// `want_usd`, what you give `give_usd`.
pub fn market_note(give_usd: f64, want_usd: f64) -> String {
    let pct = (want_usd / give_usd - 1.0) * 100.0;
    if pct.abs() < 1.0 {
        "Close to the market rate.".into()
    } else if pct > 0.0 {
        format!("You ask {:.0}% more than the market rate; solvers may not answer.", pct)
    } else {
        format!("You would get {:.0}% less than the market rate.", -pct)
    }
}

/// Why a price is not good enough to compare with: one source only, sources
/// that disagree, or an old one. None when it is fine.
pub fn doubt(prices: &Value, symbol: &str, now: u64) -> Option<String> {
    let p = &prices["prices"][symbol];
    if p["disagree"] == true {
        return Some(format!("the sources disagree on {symbol}"));
    }
    if p["updated_at"].as_u64().is_some_and(|at| now.saturating_sub(at) > 15 * 60) {
        return Some(format!("the {symbol} price is old"));
    }
    if p["sources"].as_array().is_some_and(|s| s.len() < 2) {
        return Some(format!("{symbol} has one source"));
    }
    None
}

/// The form's USD lines: under each amount, and the note below the form.
#[derive(Default, Debug, PartialEq)]
pub struct Lines {
    pub give: String,
    pub want: String,
    pub note: String,
}

pub fn lines(prices: &Value, give: (&str, &str), want: (&str, &str), now: u64) -> Lines {
    let (gp, wp) = (price(prices, give.0), price(prices, want.0));
    let gv = gp.and_then(|p| value(give.1, p));
    let wv = wp.and_then(|p| value(want.1, p));
    let mut rates: Vec<String> = [(give.0, gp), (want.0, wp)]
        .into_iter()
        .filter_map(|(s, p)| Some(unit(s, p?)))
        .collect();
    rates.dedup();
    let mut note = rates.join(" · ");
    let doubts: Vec<String> = [give.0, want.0].iter().filter_map(|s| doubt(prices, s, now)).collect();
    if let (Some(g), Some(w), false) = (gv, wv, doubts.iter().any(|d| !d.contains("one source"))) {
        note = format!("{note}. {}", market_note(g, w));
        if !doubts.is_empty() {
            note = format!("{note} Note: {}.", doubts.join(", "));
        }
    } else if gv.is_some() && wv.is_some() {
        note = format!("{note}. No comparison: {}.", doubts.join(", "));
    } else if gp.is_none() || wp.is_none() {
        let missing = if gp.is_none() { give.0 } else { want.0 };
        note = if note.is_empty() { format!("No price for {missing}.") } else { format!("{note}. No price for {missing}.") };
    }
    Lines { give: gv.map(format).unwrap_or_default(), want: wv.map(format).unwrap_or_default(), note }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prices() -> Value {
        json!({"prices": {"NPT": {"usd": "0.15"}, "USDC": {"usd": "1"}, "BTC": {"usd": "64505.9"}}})
    }

    #[test]
    fn amounts_get_usd_and_a_market_note() {
        let l = lines(&prices(), ("USDC", "250"), ("NPT", "1500"), 0);
        assert_eq!(l.give, "≈ $250.00");
        assert_eq!(l.want, "≈ $225.00");
        assert_eq!(l.note, "1 USDC ≈ $1.00 · 1 NPT ≈ $0.1500. You would get 10% less than the market rate.");
        let l = lines(&prices(), ("BTC", "0.234234"), ("NPT", "14"), 0);
        assert_eq!(l.give, "≈ $15,109.47");
        assert_eq!(l.want, "≈ $2.10");
        assert!(l.note.ends_with("You would get 100% less than the market rate."));
    }

    #[test]
    fn doubtful_prices_say_so_and_give_no_comparison() {
        let p = json!({"prices": {
            "NPT": {"usd": "0.15", "sources": ["coingecko"], "updated_at": 1000},
            "USDC": {"usd": "1", "sources": ["coingecko", "kraken"], "updated_at": 1000},
            "ETH": {"usd": "1800", "sources": ["coingecko", "kraken"], "disagree": true, "updated_at": 1000}}});
        let l = lines(&p, ("USDC", "250"), ("NPT", "1500"), 1000);
        assert!(l.note.ends_with("You would get 10% less than the market rate. Note: NPT has one source."), "{}", l.note);
        let l = lines(&p, ("ETH", "1"), ("NPT", "1500"), 1000);
        assert!(l.note.ends_with("No comparison: the sources disagree on ETH, NPT has one source."), "{}", l.note);
        let l = lines(&p, ("USDC", "250"), ("NPT", "1500"), 1000 + 3600);
        assert!(l.note.ends_with("No comparison: the USDC price is old, the NPT price is old."), "{}", l.note);
    }

    #[test]
    fn asking_more_and_fair_and_missing_prices_read_well() {
        assert_eq!(market_note(100.0, 130.0), "You ask 30% more than the market rate; solvers may not answer.");
        assert_eq!(market_note(100.0, 100.4), "Close to the market rate.");
        let l = lines(&prices(), ("ELA", "5"), ("NPT", "10"), 0);
        assert_eq!((l.give.as_str(), l.want.as_str()), ("", "≈ $1.50"));
        assert_eq!(l.note, "1 NPT ≈ $0.1500. No price for ELA.");
        let l = lines(&prices(), ("USDC", ""), ("NPT", "abc"), 0);
        assert_eq!((l.give.as_str(), l.want.as_str()), ("", ""));
        assert_eq!(format(0.004), "< $0.01");
        assert_eq!(unit("BTC", 64505.9), "1 BTC ≈ $64,506");
    }
}
