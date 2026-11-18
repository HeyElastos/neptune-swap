//! Standing offers in the app: the public list (verified, deduplicated,
//! bounded), how an offer reads, and the offer a solver's form describes.

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use intents_protocol::{decode, mul_div, Amount, Asset, Net, Offer, MAX_MESSAGE_BYTES, MAX_TTL_SECS, PROTOCOL_VERSION};

use crate::intent::{AssetDef, ASSETS};

/// At most this many offers from one solver, and in all: a flood of offers
/// costs the flooder its own slots, not everyone's screen.
pub const PER_SOLVER: usize = 5;
pub const TOTAL: usize = 200;

#[derive(Clone)]
pub struct Listed {
    pub offer: Offer,
    pub solver: String,
}

#[derive(Default)]
pub struct Board {
    offers: HashMap<(String, String), Listed>,
    /// Withdrawn offers, until no earlier copy can still be live: a replayed
    /// or late copy does not bring one back.
    withdrawn: HashMap<(String, String), u64>,
    /// Solvers this Home has finished swaps with: their offers are listed first and never
    /// pushed out by others, so a flood of new identities cannot hide them.
    trusted: std::collections::HashSet<String>,
}

impl Board {
    /// Takes one message from the topic: base64url of a signed offer. Anything
    /// that does not verify for `net` at `now` is dropped unread.
    /// Marks `solver` as one this Home has finished a swap with.
    pub fn trust(&mut self, solver: &str) {
        self.trusted.insert(solver.to_string());
    }

    pub fn is_trusted(&self, solver: &str) -> bool {
        self.trusted.contains(solver)
    }

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
            self.withdrawn.insert(key, now + MAX_TTL_SECS);
            return Ok(());
        }
        if self.withdrawn.contains_key(&key) {
            return Err("withdrawn".into());
        }
        if let Some(old) = self.offers.get(&key) {
            // A copy may only live longer; new terms need a new id, so the row a
            // user reads is the offer it takes.
            let same_terms = Offer { expires_at: old.offer.expires_at, ..decoded.body.clone() } == old.offer;
            if !same_terms {
                return Err("changed terms under the same id".into());
            }
            if decoded.body.expires_at <= old.offer.expires_at {
                return Ok(()); // an older copy
            }
        } else {
            let mine = self.offers.keys().filter(|(s, _)| *s == solver).count();
            if mine >= PER_SOLVER {
                return Err("this solver has enough offers listed".into());
            }
            if self.offers.len() >= TOTAL {
                self.prune(now);
            }
            if self.offers.len() >= TOTAL {
                // Make room: the untrusted offer that ends soonest goes. A trusted solver's
                // offer is never pushed out; when only those are left, a new untrusted one waits.
                let evict = self.offers.iter().filter(|(k, _)| !self.trusted.contains(&k.0)).min_by_key(|(_, l)| l.offer.expires_at).map(|(k, _)| k.clone());
                match evict {
                    Some(k) => {
                        self.offers.remove(&k);
                    }
                    None => return Err("the board is full of trusted solvers' offers".into()),
                }
            }
        }
        self.offers.insert(key, Listed { offer: decoded.body, solver });
        Ok(())
    }

    pub fn prune(&mut self, now: u64) {
        self.offers.retain(|_, l| l.offer.expires_at > now);
        self.withdrawn.retain(|_, until| *until > now);
    }

    /// The live offers a user who gives `give` and wants `want` can take,
    /// best rate for the user first. Assets match on chain and symbol.
    pub fn matching(&self, give: &Asset, want: &Asset, now: u64) -> Vec<&Listed> {
        let mut rows: Vec<&Listed> = self
            .offers
            .values()
            .filter(|l| l.offer.expires_at > now && l.offer.want == *give && l.offer.give == *want)
            .collect();
        // Solvers this Home has finished swaps with first, then more of `want` per unit of `give`.
        rows.sort_by(|a, b| {
            let trust = self.trusted.contains(&b.solver).cmp(&self.trusted.contains(&a.solver));
            trust.then(user_rate(&b.offer).partial_cmp(&user_rate(&a.offer)).unwrap_or(std::cmp::Ordering::Equal))
        });
        rows
    }

    /// The offer as listed now, if it still is.
    pub fn get(&self, solver: &str, id: &str) -> Option<&Listed> {
        self.offers.get(&(solver.to_string(), id.to_string()))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.offers.len()
    }
}

/// The asset the form's row `index` stands for, if it can be swapped.
pub fn asset_at(index: usize) -> Option<Asset> {
    let d = ASSETS.get(index)?;
    Some(Asset { chain: d.chain?, symbol: d.symbol.into() })
}

fn def(symbol: &str) -> Option<&'static AssetDef> {
    ASSETS.iter().find(|a| a.symbol == symbol)
}

/// Coins in `units` of `asset` (display only), by chain and symbol: two rows may share a symbol.
pub fn coins(units: u128, asset: &Asset) -> f64 {
    let Some(d) = crate::intent::row_of(asset).map(|i| &ASSETS[i]) else { return 0.0 };
    units as f64 / (d.unit as f64 * 10f64.powi(d.decimals as i32))
}

/// What the taker gets per coin it gives.
fn user_rate(o: &Offer) -> f64 {
    let give = coins(o.rate_give.units(), &o.give);
    let want = coins(o.rate_want.units(), &o.want);
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
    let (g, w) = (coins(o.rate_give.units(), &o.give), coins(o.rate_want.units(), &o.want));
    if o.give.is_npt() { (w / g, &o.want.symbol) } else { (g / w, &o.give.symbol) }
}

/// The offer's limits in NPT.
pub fn npt_limits(o: &Offer) -> (f64, f64) {
    let (min, max) = (coins(o.min_give.units(), &o.give), coins(o.max_give.units(), &o.give));
    if o.give.is_npt() {
        (min, max)
    } else {
        let (p, _) = npt_price(o);
        (min / p, max / p)
    }
}

/// "did:key:z6Mk" is the same for every Ed25519 key: what tells solvers apart
/// is shown as 8 + 8 characters after it (58^16 tries to imitate).
pub fn short_did(did: &str) -> String {
    match did.strip_prefix("did:key:z6Mk") {
        Some(k) if k.len() > 20 => format!("{}…{}", &k[..8], &k[k.len() - 8..]),
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
        format!("{}–{} NPT · {} · {} min", fmt(min), fmt(max), short_did(&l.solver), left.max(1)),
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
    let other = def(form.other).filter(|d| d.chain.is_some()).ok_or("Pick ELA, ETH, USDC or BTC.")?;
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
    use intents_protocol::{DidKey, Signed};

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
        assert_eq!(offer_from(&ela, "0".repeat(32), NOW).unwrap().want.chain, intents_protocol::Chain::Esc, "ELA trades on ESC");
        let unknown = SolverForm { other: "DOGE", ..form(true) };
        assert!(offer_from(&unknown, "0".repeat(32), NOW).is_err());
        let words = SolverForm { price: "cheap", ..form(true) };
        assert_eq!(offer_from(&words, "0".repeat(32), NOW).unwrap_err(), "The price is a number like 0.15 or 1000.");
    }

    #[test]
    fn numbers_and_offers_read_well() {
        assert_eq!(fmt(0.15), "0.15");
        assert_eq!(fmt(10_000.0), "10,000");
        assert_eq!(fmt(64_505.9), "64,505.9");
        assert_eq!(fmt(0.000123456789), "0.000123457");
        let did = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK";
        assert_eq!(short_did(did), "haXgBZDv…EGta2doK");
        let l = Listed { offer: offer_from(&form(true), "0".repeat(32), NOW).unwrap(), solver: did.into() };
        let (title, detail) = describe(&l, NOW);
        assert_eq!(title, "Sells NPT · 1 NPT = 0.15 USDC");
        assert_eq!(detail, "10–10,000 NPT · haXgBZDv…EGta2doK · 30 min");
    }

    fn usdc() -> Asset {
        asset_at(3).unwrap()
    }

    fn npt() -> Asset {
        Asset::npt()
    }

    #[test]
    fn the_board_verifies_dedupes_withdraws_and_bounds() {
        let mut board = Board::default();
        let o = offer_from(&form(true), "1".repeat(32), NOW).unwrap();
        board.ingest(&published(&o, 1), NOW, Net::Main).unwrap();
        // The same offer again, and an older copy: still one, still the newest.
        board.ingest(&published(&o, 1), NOW, Net::Main).unwrap();
        let older = Offer { expires_at: NOW + 60, ..o.clone() };
        board.ingest(&published(&older, 1), NOW, Net::Main).unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board.matching(&usdc(), &npt(), NOW)[0].offer.expires_at, o.expires_at);
        assert!(board.matching(&npt(), &usdc(), NOW).is_empty(), "the other direction");
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
        assert_eq!(board.matching(&usdc(), &npt(), NOW)[0].offer.id, "2".repeat(32));
        // Five offers per solver at most.
        for i in 0..6u8 {
            let more = Offer { id: format!("{:032x}", 100 + i), ..o.clone() };
            let _ = board.ingest(&published(&more, 4), NOW, Net::Main);
        }
        assert_eq!(board.matching(&usdc(), &npt(), NOW).iter().filter(|l| l.offer.id.starts_with("0000")).count(), PER_SOLVER);
        // Withdrawn, then expired: gone.
        board.ingest(&published(&Offer { withdrawn: true, ..cheaper.clone() }, 3), NOW, Net::Main).unwrap();
        assert!(board.get(&DidKey::new(SigningKey::from_bytes(&[3; 32]).verifying_key()).to_string(), &"2".repeat(32)).is_none());
        board.prune(NOW + 1801);
        assert_eq!(board.len(), 0);
    }

    #[test]
    fn a_withdrawn_offer_stays_gone_and_terms_never_change_under_an_id() {
        let mut board = Board::default();
        let o = offer_from(&form(true), "3".repeat(32), NOW).unwrap();
        board.ingest(&published(&o, 5), NOW, Net::Main).unwrap();
        board.ingest(&published(&Offer { withdrawn: true, ..o.clone() }, 5), NOW, Net::Main).unwrap();
        // A replayed earlier copy, or a later one, does not bring it back.
        assert!(board.ingest(&published(&o, 5), NOW + 10, Net::Main).is_err());
        assert!(board.ingest(&published(&Offer { expires_at: NOW + 3000, ..o.clone() }, 5), NOW + 10, Net::Main).is_err());
        assert_eq!(board.len(), 0);
        // Same id, worse rate: refused; the listed terms stay.
        let p = offer_from(&form(true), "4".repeat(32), NOW).unwrap();
        board.ingest(&published(&p, 6), NOW, Net::Main).unwrap();
        let worse = Offer { rate_want: Amount::new(300_000), expires_at: NOW + 1900, ..p.clone() };
        assert!(board.ingest(&published(&worse, 6), NOW, Net::Main).is_err());
        assert_eq!(board.matching(&usdc(), &npt(), NOW)[0].offer, p);
        // Longer life with the same terms is fine.
        board.ingest(&published(&Offer { expires_at: NOW + 1900, ..p.clone() }, 6), NOW, Net::Main).unwrap();
        assert_eq!(board.matching(&usdc(), &npt(), NOW)[0].offer.expires_at, NOW + 1900);
    }

    #[test]
    fn a_full_board_makes_room_and_assets_match_on_chain_too() {
        let mut board = Board::default();
        for seed in 0..(TOTAL as u32 / PER_SOLVER as u32) {
            for i in 0..PER_SOLVER as u32 {
                let o = Offer { id: format!("{:032x}", seed * 10 + i), expires_at: NOW + 600 + u64::from(seed), ..offer_from(&form(true), "0".repeat(32), NOW).unwrap() };
                board.ingest(&published(&o, 10 + seed as u8), NOW, Net::Main).unwrap();
            }
        }
        assert_eq!(board.len(), TOTAL);
        let fresh = offer_from(&form(true), "9".repeat(32), NOW).unwrap();
        board.ingest(&published(&fresh, 99), NOW, Net::Main).unwrap();
        assert_eq!(board.len(), TOTAL, "the soonest-ending offer made room");
        assert!(board.matching(&usdc(), &npt(), NOW).iter().any(|l| l.offer.id == "9".repeat(32)));
        let other_chain = Asset { chain: intents_protocol::Chain::Bitcoin, symbol: "USDC".into() };
        assert!(board.matching(&other_chain, &npt(), NOW).is_empty());
    }

    #[test]
    fn a_flood_of_new_identities_never_pushes_out_a_trusted_solver() {
        let mut board = Board::default();
        let trusted = offer_from(&form(true), "7".repeat(32), NOW).unwrap();
        board.ingest(&published(&trusted, 7), NOW, Net::Main).unwrap();
        let solver = board.matching(&usdc(), &npt(), NOW)[0].solver.clone();
        board.trust(&solver);
        // 300 spam offers from 60 identities, all ending later than the trusted one.
        for seed in 0..60u32 {
            for i in 0..PER_SOLVER as u32 {
                let o = Offer { id: format!("{:032x}", 1000 + seed * 10 + i), expires_at: NOW + 3000, rate_want: Amount::new(1), ..trusted.clone() };
                let _ = board.ingest(&published(&o, 100 + seed as u8), NOW, Net::Main);
            }
        }
        assert_eq!(board.len(), TOTAL);
        let rows = board.matching(&usdc(), &npt(), NOW);
        assert_eq!(rows[0].solver, solver, "listed first, even against better rates");
        assert!(board.is_trusted(&solver));
    }
}
