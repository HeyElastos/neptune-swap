//! Standing offers in the app: the public list (verified, deduplicated,
//! bounded), how an offer reads, and the offer a solver's form describes.

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use intents_protocol::{decode, mul_div, Amount, Asset, Net, Offer, MAX_MESSAGE_BYTES, PROTOCOL_VERSION};

use crate::intent::{AssetDef, ASSETS};

/// At most this many offers from one solver, and in all: a flood of offers
/// costs the flooder its own slots, not everyone's screen.
pub const PER_SOLVER: usize = 5;
pub const TOTAL: usize = 200;

pub struct Listed {
    pub offer: Offer,
    pub solver: String,
}

#[derive(Default)]
pub struct Board {
    offers: HashMap<(String, String), Listed>,
}

impl Board {
    /// Takes one message from the topic: base64url of a signed offer. Anything
    /// that does not verify for `net` at `now` is dropped unread.
    pub fn ingest(&mut self, message: &str, now: u64, net: Net) -> Result<(), String> {
        if message.len() > MAX_MESSAGE_BYTES * 4 / 3 + 4 {
            return Err("too large".into());
        }
        let bytes = URL_SAFE_NO_PAD.decode(message.trim()).map_err(|_| "not base64url".to_string())?;
        let decoded = decode::<Offer>(&bytes, now, net).map_err(|e| e.to_string())?;
        let solver = decoded.signer.to_string();
        let key = (solver.clone(), decoded.body.id.clone());
        if decoded.body.withdrawn {
            self.offers.remove(&key);
            return Ok(());
        }
        if let Some(old) = self.offers.get(&key) {
            if old.offer.expires_at >= decoded.body.expires_at {
                return Ok(()); // an older copy
            }
        } else {
            let mine = self.offers.keys().filter(|(s, _)| *s == solver).count();
            if mine >= PER_SOLVER {
                return Err("this solver has enough offers listed".into());
            }
            if self.offers.len() >= TOTAL {
                self.prune(now);
                if self.offers.len() >= TOTAL {
                    return Err("the list is full".into());
                }
            }
        }
        self.offers.insert(key, Listed { offer: decoded.body, solver });
        Ok(())
    }

    pub fn prune(&mut self, now: u64) {
        self.offers.retain(|_, l| l.offer.expires_at > now);
    }

    /// The live offers a user who gives `give` and wants `want` can take,
    /// best rate for the user first.
    pub fn matching(&self, give: &str, want: &str, now: u64) -> Vec<&Listed> {
        let mut rows: Vec<&Listed> = self
            .offers
            .values()
            .filter(|l| l.offer.expires_at > now && l.offer.want.symbol == give && l.offer.give.symbol == want)
            .collect();
        // More of `want` per unit of `give` first.
        rows.sort_by(|a, b| user_rate(&b.offer).partial_cmp(&user_rate(&a.offer)).unwrap_or(std::cmp::Ordering::Equal));
        rows
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.offers.len()
    }
}

fn def(symbol: &str) -> Option<&'static AssetDef> {
    ASSETS.iter().find(|a| a.symbol == symbol)
}

/// Coins in `units` of `symbol` (display only).
pub fn coins(units: u128, symbol: &str) -> f64 {
    let Some(d) = def(symbol) else { return 0.0 };
    units as f64 / (d.unit as f64 * 10f64.powi(d.decimals as i32))
}

/// What the taker gets per coin it gives.
fn user_rate(o: &Offer) -> f64 {
    let give = coins(o.rate_give.units(), &o.give.symbol);
    let want = coins(o.rate_want.units(), &o.want.symbol);
    if want > 0.0 { give / want } else { 0.0 }
}

/// Up to six significant digits, no trailing zeros, thousands grouped.
pub fn fmt(x: f64) -> String {
    if !x.is_finite() || x <= 0.0 {
        return "0".into();
    }
    let decimals = (5 - x.log10().floor() as i32).clamp(0, 12) as usize;
    let text = format!("{x:.decimals$}");
    let (int, frac) = text.split_once('.').map_or((text.as_str(), ""), |(i, f)| (i, f));
    let frac = frac.trim_end_matches('0');
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if frac.is_empty() { grouped } else { format!("{grouped}.{frac}") }
}

/// The price of one NPT in the offer's other asset.
pub fn npt_price(o: &Offer) -> (f64, &str) {
    let (g, w) = (coins(o.rate_give.units(), &o.give.symbol), coins(o.rate_want.units(), &o.want.symbol));
    if o.give.is_npt() { (w / g, &o.want.symbol) } else { (g / w, &o.give.symbol) }
}

/// The offer's limits in NPT.
pub fn npt_limits(o: &Offer) -> (f64, f64) {
    let (min, max) = (coins(o.min_give.units(), &o.give.symbol), coins(o.max_give.units(), &o.give.symbol));
    if o.give.is_npt() {
        (min, max)
    } else {
        let (p, _) = npt_price(o);
        (min / p, max / p)
    }
}

/// "did:key:z6Mk…AbCd".
pub fn short_did(did: &str) -> String {
    match did.strip_prefix("did:key:") {
        Some(k) if k.len() > 10 => format!("did:key:{}…{}", &k[..4], &k[k.len() - 4..]),
        _ => did.to_string(),
    }
}

/// How an offer reads in the list: the title (what you get and at what
/// price) and the detail (limits, solver, time left).
pub fn describe(l: &Listed, now: u64) -> (String, String) {
    let o = &l.offer;
    let (price, other) = npt_price(o);
    let (min, max) = npt_limits(o);
    let side = if o.give.is_npt() { "Sells NPT" } else { "Buys NPT" };
    let left = o.expires_at.saturating_sub(now) / 60;
    (
        format!("{side} · 1 NPT = {} {other}", fmt(price)),
        format!("{}–{} NPT · {} · {} min", fmt(min), fmt(max), short_did(&l.solver).trim_start_matches("did:key:"), left.max(1)),
    )
}

/// The solver's form: which side it takes, the other asset, its price per
/// NPT in that asset, its limits in NPT, and how long the offer lives.
pub struct SolverForm<'a> {
    pub gives_npt: bool,
    pub other: &'a str,
    pub price: &'a str,
    pub min_npt: &'a str,
    pub max_npt: &'a str,
    pub ttl_secs: u64,
}

/// The offer the form describes, or a sentence saying what is wrong.
pub fn offer_from(form: &SolverForm, id: String, now: u64) -> Result<Offer, String> {
    use intents_protocol::Message as _;
    let npt = def("NPT").ok_or("no NPT")?;
    let other = def(form.other).filter(|d| d.chain.is_some()).ok_or("Pick ETH, USDC or BTC.")?;
    let parse = |text: &str, d: &AssetDef, what: &str| {
        Amount::parse_decimal(text, d.decimals, d.unit).map_err(|_| format!("{what} is a number like 0.15 or 1000."))
    };
    let one_npt = parse("1", npt, "One NPT")?;
    let price = parse(form.price, other, "The price")?;
    let (min, max) = (parse(form.min_npt, npt, "The minimum")?, parse(form.max_npt, npt, "The maximum")?);
    let npt_asset = Asset::npt();
    let other_asset = Asset { chain: other.chain.ok_or("no chain")?, symbol: other.symbol.into() };
    let in_other = |a: Amount, up: bool| {
        mul_div(a.units(), price.units(), one_npt.units(), up).map(Amount::new).ok_or("That amount is too large.".to_string())
    };
    let offer = if form.gives_npt {
        Offer {
            v: PROTOCOL_VERSION,
            id,
            give: npt_asset,
            want: other_asset,
            rate_give: one_npt,
            rate_want: price,
            min_give: min,
            max_give: max,
            expires_at: now + form.ttl_secs,
            withdrawn: false,
        }
    } else {
        Offer {
            v: PROTOCOL_VERSION,
            id,
            give: other_asset,
            want: npt_asset,
            rate_give: price,
            rate_want: one_npt,
            min_give: in_other(min, true)?,
            max_give: in_other(max, false)?,
            expires_at: now + form.ttl_secs,
            withdrawn: false,
        }
    };
    offer.validate(now).map_err(|e| match e {
        intents_protocol::Error::Invalid { field: "max_give", .. } => "The maximum is below the minimum.".to_string(),
        intents_protocol::Error::Invalid { reason, .. } if reason.contains("positive") => "Price and limits must be above zero.".to_string(),
        other => other.to_string(),
    })?;
    Ok(offer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use intents_protocol::Signed;

    fn published(o: &Offer, seed: u8) -> String {
        URL_SAFE_NO_PAD.encode(Signed::sign(o, &SigningKey::from_bytes(&[seed; 32]), Net::Main).unwrap().encode().unwrap())
    }

    const NOW: u64 = 1_800_000_000;

    fn form(gives_npt: bool) -> SolverForm<'static> {
        SolverForm { gives_npt, other: "USDC", price: "0.15", min_npt: "10", max_npt: "10000", ttl_secs: 1800 }
    }

    #[test]
    fn a_form_makes_the_offer_both_ways() {
        let sell = offer_from(&form(true), "0".repeat(32), NOW).unwrap();
        assert_eq!((sell.give.symbol.as_str(), sell.rate_want.units()), ("NPT", 150_000));
        let buy = offer_from(&form(false), "0".repeat(32), NOW).unwrap();
        assert_eq!((buy.give.symbol.as_str(), buy.min_give.units(), buy.max_give.units()), ("USDC", 1_500_000, 1_500_000_000));
        assert_eq!(npt_price(&buy), (0.15, "USDC"));
        let (min, max) = npt_limits(&buy);
        assert!((min - 10.0).abs() < 1e-9 && (max - 10_000.0).abs() < 1e-6);
        let bad = SolverForm { min_npt: "20000", ..form(true) };
        assert_eq!(offer_from(&bad, "0".repeat(32), NOW).unwrap_err(), "The maximum is below the minimum.");
        let ela = SolverForm { other: "ELA", ..form(true) };
        assert!(offer_from(&ela, "0".repeat(32), NOW).is_err());
        let words = SolverForm { price: "cheap", ..form(true) };
        assert_eq!(offer_from(&words, "0".repeat(32), NOW).unwrap_err(), "The price is a number like 0.15 or 1000.");
    }

    #[test]
    fn numbers_and_offers_read_well() {
        assert_eq!(fmt(0.15), "0.15");
        assert_eq!(fmt(10_000.0), "10,000");
        assert_eq!(fmt(64_505.9), "64,505.9");
        assert_eq!(fmt(0.000123456789), "0.000123457");
        assert_eq!(short_did("did:key:z6MkABCDEFGHIJKLmnop"), "did:key:z6Mk…mnop");
        let l = Listed { offer: offer_from(&form(true), "0".repeat(32), NOW).unwrap(), solver: "did:key:z6MkABCDEFGHIJKLmnop".into() };
        let (title, detail) = describe(&l, NOW);
        assert_eq!(title, "Sells NPT · 1 NPT = 0.15 USDC");
        assert_eq!(detail, "10–10,000 NPT · z6Mk…mnop · 30 min");
    }

    #[test]
    fn the_board_verifies_dedupes_withdraws_and_bounds() {
        let mut board = Board::default();
        let o = offer_from(&form(true), "1".repeat(32), NOW).unwrap();
        board.ingest(&published(&o, 1), NOW, Net::Main).unwrap();
        // The same offer again, and an older copy: still one.
        board.ingest(&published(&o, 1), NOW, Net::Main).unwrap();
        let older = Offer { expires_at: NOW + 60, ..o.clone() };
        board.ingest(&published(&older, 1), NOW, Net::Main).unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board.matching("USDC", "NPT", NOW)[0].offer.expires_at, o.expires_at);
        assert!(board.matching("NPT", "USDC", NOW).is_empty(), "the other direction");
        // Junk, another network and a changed body are dropped.
        assert!(board.ingest("not base64!", NOW, Net::Main).is_err());
        assert!(board.ingest(&published(&o, 1), NOW, Net::Test).is_err());
        let mut forged = URL_SAFE_NO_PAD.decode(published(&o, 2)).unwrap();
        let at = forged.windows(6).position(|w| w == b"150000").unwrap();
        forged[at + 1] = b'6'; // 150000 -> 160000
        assert!(board.ingest(&URL_SAFE_NO_PAD.encode(forged), NOW, Net::Main).is_err());
        // A better rate from another solver comes first.
        let cheaper = offer_from(&SolverForm { price: "0.10", ..form(true) }, "2".repeat(32), NOW).unwrap();
        board.ingest(&published(&cheaper, 3), NOW, Net::Main).unwrap();
        assert_eq!(board.matching("USDC", "NPT", NOW)[0].offer.id, "2".repeat(32));
        // Five offers per solver at most.
        for i in 0..6u8 {
            let more = Offer { id: format!("{:032x}", 100 + i), ..o.clone() };
            let _ = board.ingest(&published(&more, 4), NOW, Net::Main);
        }
        assert_eq!(board.matching("USDC", "NPT", NOW).iter().filter(|l| l.offer.id.starts_with("0000")).count(), PER_SOLVER);
        // Withdrawn, then expired: gone.
        board.ingest(&published(&Offer { withdrawn: true, ..cheaper }, 3), NOW, Net::Main).unwrap();
        assert!(board.matching("USDC", "NPT", NOW).iter().all(|l| l.offer.id != "2".repeat(32)));
        board.prune(NOW + 1801);
        assert_eq!(board.len(), 0);
    }
}
