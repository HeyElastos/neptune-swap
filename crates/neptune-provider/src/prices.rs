//! Reference prices in USD from public sources, for showing what a swap is
//! worth. Display only: no amount, quote or lock depends on them.
//!
//! The sources see this computer's address and when it asks (not what it
//! swaps), so the provider asks only when its configuration allows it
//! (`price_sources`), and the Runtime asks the user first (the gateway's
//! approval for `elastos://neptune/prices`). Answers are cached for a minute.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(8);
const CACHE: Duration = Duration::from_secs(60);
/// Two sources further apart than this (percent) are shown as disagreeing.
const DISAGREE_PCT: f64 = 2.0;

/// The swap's assets: our symbol, CoinGecko id, Kraken pair (result key).
const ASSETS: [(&str, &str, Option<(&str, &str)>); 4] = [
    ("NPT", "neptune-cash", None),
    ("BTC", "bitcoin", Some(("XBTUSD", "XXBTZUSD"))),
    ("ETH", "ethereum", Some(("ETHUSD", "XETHZUSD"))),
    ("USDC", "usd-coin", Some(("USDCUSD", "USDCUSD"))),
];

pub const COINGECKO: &str = "https://api.coingecko.com/api/v3/simple/price?ids=neptune-cash,bitcoin,ethereum,usd-coin&vs_currencies=usd&include_last_updated_at=true";
pub const KRAKEN: &str = "https://api.kraken.com/0/public/Ticker?pair=XBTUSD,ETHUSD,USDCUSD";

#[derive(Default)]
pub struct Prices {
    cached: Option<(Instant, Value)>,
}

impl Prices {
    /// The current prices, from the cache when it is fresh.
    pub fn get(&mut self, fetch: impl Fn(&str) -> Result<Value, String>) -> Result<Value, String> {
        if let Some((at, value)) = &self.cached {
            if at.elapsed() < CACHE {
                return Ok(value.clone());
            }
        }
        let gecko = fetch(COINGECKO);
        let kraken = fetch(KRAKEN);
        if let (Err(a), Err(b)) = (&gecko, &kraken) {
            return Err(format!("no price source answered (CoinGecko: {a}; Kraken: {b})"));
        }
        let value = combine(gecko.ok().as_ref().map(coingecko), kraken.ok().as_ref().map(kraken_prices), now_secs());
        self.cached = Some((Instant::now(), value.clone()));
        Ok(value)
    }
}

pub fn fetch_json(url: &str) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let reply = agent.get(url).call().map_err(|e| e.to_string())?;
    serde_json::from_str(&reply.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Symbol → (USD, updated at) from CoinGecko's `simple/price`.
fn coingecko(reply: &Value) -> BTreeMap<&'static str, (f64, Option<u64>)> {
    ASSETS
        .iter()
        .filter_map(|(symbol, id, _)| {
            let usd = reply[*id]["usd"].as_f64().filter(|p| p.is_finite() && *p > 0.0)?;
            Some((*symbol, (usd, reply[*id]["last_updated_at"].as_u64())))
        })
        .collect()
}

/// Symbol → last trade price in USD from Kraken's `Ticker`.
fn kraken_prices(reply: &Value) -> BTreeMap<&'static str, (f64, Option<u64>)> {
    if reply["error"].as_array().is_some_and(|e| !e.is_empty()) {
        return BTreeMap::new();
    }
    ASSETS
        .iter()
        .filter_map(|(symbol, _, pair)| {
            let (_, key) = (*pair)?;
            let last = reply["result"][key]["c"][0].as_str()?.parse::<f64>().ok().filter(|p| p.is_finite() && *p > 0.0)?;
            Some((*symbol, (last, None)))
        })
        .collect()
}

/// One row per asset any source priced: the mean of the sources, which
/// sources, and whether they disagree.
fn combine(
    gecko: Option<BTreeMap<&'static str, (f64, Option<u64>)>>,
    kraken: Option<BTreeMap<&'static str, (f64, Option<u64>)>>,
    now: u64,
) -> Value {
    let mut prices = serde_json::Map::new();
    for (symbol, _, _) in ASSETS {
        let mut seen = vec![];
        if let Some(&(usd, at)) = gecko.as_ref().and_then(|g| g.get(symbol)) {
            seen.push(("coingecko", usd, at));
        }
        if let Some(&(usd, at)) = kraken.as_ref().and_then(|k| k.get(symbol)) {
            seen.push(("kraken", usd, at));
        }
        if seen.is_empty() {
            continue;
        }
        let mean = seen.iter().map(|s| s.1).sum::<f64>() / seen.len() as f64;
        let (lo, hi) = seen.iter().fold((f64::MAX, 0f64), |(lo, hi), s| (lo.min(s.1), hi.max(s.1)));
        let spread_pct = (hi - lo) / mean * 100.0;
        prices.insert(
            symbol.to_string(),
            json!({
                "usd": format_usd(mean),
                "sources": seen.iter().map(|s| s.0).collect::<Vec<_>>(),
                "spread_pct": (spread_pct * 100.0).round() / 100.0,
                "disagree": spread_pct > DISAGREE_PCT,
                "updated_at": seen.iter().filter_map(|s| s.2).max().unwrap_or(now),
            }),
        );
    }
    json!({ "prices": prices, "fetched_at": now, "currency": "USD" })
}

/// Enough significant digits for a cheap coin, few for an expensive one.
fn format_usd(usd: f64) -> String {
    let decimals = if usd >= 1000.0 { 2 } else if usd >= 1.0 { 4 } else { 6 };
    let text = format!("{usd:.decimals$}");
    if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gecko() -> Value {
        json!({"neptune-cash":{"usd":0.159867,"last_updated_at":1794843912},"bitcoin":{"usd":64328.0,"last_updated_at":1794843904},
               "ethereum":{"usd":1811.84,"last_updated_at":1794843891},"usd-coin":{"usd":0.999611,"last_updated_at":1794843904}})
    }

    fn kraken() -> Value {
        json!({"error":[],"result":{"XETHZUSD":{"c":["1816.72000","0.1"]},"USDCUSD":{"c":["0.99980000","5"]},"XXBTZUSD":{"c":["64444.10000","0.01"]}}})
    }

    #[test]
    fn both_sources_combine_and_npt_comes_from_coingecko() {
        let v = combine(Some(coingecko(&gecko())), Some(kraken_prices(&kraken())), 1);
        let p = &v["prices"];
        assert_eq!(p["NPT"]["usd"], "0.159867");
        assert_eq!(p["NPT"]["sources"], json!(["coingecko"]));
        assert_eq!(p["BTC"]["sources"], json!(["coingecko", "kraken"]));
        assert_eq!(p["BTC"]["usd"], "64386.05");
        assert_eq!(p["BTC"]["disagree"], false);
        assert_eq!(p["USDC"]["usd"], "0.999706");
    }

    #[test]
    fn far_apart_sources_are_flagged_and_one_source_is_enough() {
        let mut k = kraken();
        k["result"]["XETHZUSD"]["c"][0] = json!("2000");
        let v = combine(Some(coingecko(&gecko())), Some(kraken_prices(&k)), 1);
        assert_eq!(v["prices"]["ETH"]["disagree"], true);
        let only_kraken = combine(None, Some(kraken_prices(&kraken())), 1);
        assert!(only_kraken["prices"].get("NPT").is_none());
        assert_eq!(only_kraken["prices"]["ETH"]["usd"], "1816.72");
    }

    #[test]
    fn junk_is_ignored_and_the_cache_holds() {
        let v = combine(Some(coingecko(&json!({"bitcoin":{"usd":-1}}))), Some(kraken_prices(&json!({"error":["EGeneral"]}))), 1);
        assert_eq!(v["prices"], json!({}));
        let calls = std::cell::Cell::new(0);
        let mut prices = Prices::default();
        let fetch = |url: &str| {
            calls.set(calls.get() + 1);
            Ok(if url == COINGECKO { gecko() } else { kraken() })
        };
        prices.get(fetch).unwrap();
        prices.get(fetch).unwrap();
        assert_eq!(calls.get(), 2, "one fetch per source, then the cache");
        assert_eq!(Prices::default().get(|_| Err("offline".into())).unwrap_err().contains("no price source"), true);
    }
}
