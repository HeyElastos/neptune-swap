//! The assets the form offers and how a filled form becomes a protocol [`Intent`].

use intents_protocol::{Amount, Asset, Chain, Error, Intent, Message, PROTOCOL_VERSION};

pub struct AssetDef {
    /// None while the asset cannot be swapped without a bridge: shown, not offered.
    pub chain: Option<Chain>,
    pub symbol: &'static str,
    /// The letter on the asset's face.
    pub letter: &'static str,
    pub network: &'static str,
    /// Digits a person may type after the point.
    pub decimals: u32,
    /// Smallest units per typed step (1 NPT = 4 * 10^30 nau).
    pub unit: u128,
}

/// NPT and ELA first: both live on UTXO chains, like Bitcoin. ELA on the Elastos main chain
/// (1 ELA = 10^8 sela) is shown but not offered yet: the main chain has no HTLC program, so it
/// joins through a scriptless swap, still being built. Never through a bridge. USDC and ETH
/// are on Base, where elastOS Wallet pays.
pub const ASSETS: [AssetDef; 6] = [
    AssetDef { chain: Some(Chain::Neptune), symbol: "NPT", letter: "N", network: "Neptune", decimals: 30, unit: 4 },
    AssetDef { chain: Some(Chain::Esc), symbol: "ELA", letter: "E", network: "Elastos ESC", decimals: 18, unit: 1 },
    AssetDef { chain: None, symbol: "ELA", letter: "E", network: "Elastos main chain · coming", decimals: 8, unit: 1 },
    AssetDef { chain: Some(Chain::Base), symbol: "USDC", letter: "U", network: "Base", decimals: 6, unit: 1 },
    AssetDef { chain: Some(Chain::Base), symbol: "ETH", letter: "Ξ", network: "Base", decimals: 18, unit: 1 },
    AssetDef { chain: Some(Chain::Bitcoin), symbol: "BTC", letter: "B", network: "Bitcoin", decimals: 8, unit: 1 },
];

/// The form opens on "USDC for NPT".
pub const DEFAULT_GIVE: usize = 3;
pub const DEFAULT_WANT: usize = 0;

/// How long solvers may answer, matching the form's segmented choice.
pub const TTL: [(u64, &str); 3] = [(300, "5 minutes"), (600, "10 minutes"), (1800, "30 minutes")];

pub struct Form<'a> {
    pub give: usize,
    pub give_amount: &'a str,
    pub want: usize,
    pub want_amount: &'a str,
    pub ttl: usize,
}

/// Builds and checks the intent the form describes. Errors are sentences for the person.
pub fn build(form: &Form, id: String, now: u64) -> Result<Intent, String> {
    let give = ASSETS.get(form.give).ok_or("Pick what you give.")?;
    let want = ASSETS.get(form.want).ok_or("Pick what you want.")?;
    if form.give == form.want {
        return Err("Pick two different assets.".into());
    }
    let chain = |asset: &AssetDef| {
        asset.chain.ok_or(format!("{} swaps on the main chain are coming: a scriptless swap, still being built.", asset.symbol))
    };
    let (give_chain, want_chain) = (chain(give)?, chain(want)?);
    let amount = |text: &str, asset: &AssetDef| {
        if text.trim().is_empty() {
            return Err("Enter both amounts.".to_string());
        }
        Amount::parse_decimal(text, asset.decimals, asset.unit).map_err(|e| match e {
            Error::Invalid { reason, .. } if reason.contains("decimals") => {
                format!("{} has at most {} decimals.", asset.symbol, asset.decimals)
            }
            Error::Invalid { reason, .. } if reason.contains("large") => "That amount is too large.".to_string(),
            _ => "Amounts are numbers like 12 or 0.5.".to_string(),
        })
    };
    let (ttl, _) = TTL.get(form.ttl).copied().unwrap_or(TTL[1]);
    let intent = Intent {
        v: PROTOCOL_VERSION,
        id,
        give: Asset { chain: give_chain, symbol: give.symbol.into() },
        give_amount: amount(form.give_amount, give)?,
        want: Asset { chain: want_chain, symbol: want.symbol.into() },
        min_want_amount: amount(form.want_amount, want)?,
        expires_at: now + ttl,
    };
    intent.validate(now).map_err(|e| match e {
        Error::Invalid { field: "give", reason } if reason.contains("NPT") => "One side must be NPT.".to_string(),
        Error::Invalid { reason, .. } if reason.contains("positive") => "Amounts must be above zero.".to_string(),
        other => other.to_string(),
    })?;
    Ok(intent)
}

/// The body as a signature covers it (keys sorted), laid out for reading.
pub fn pretty(intent: &Intent) -> String {
    let canonical = intents_protocol::to_canonical_bytes(intent).expect("intent serializes");
    let value: serde_json::Value = serde_json::from_slice(&canonical).expect("canonical JSON parses");
    serde_json::to_string_pretty(&value).expect("value serializes")
}

/// 16 random bytes as lowercase hex.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("system randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const ID: &str = "00112233445566778899aabbccddeeff";

    fn form<'a>(give: usize, give_amount: &'a str, want: usize, want_amount: &'a str) -> Form<'a> {
        Form { give, give_amount, want, want_amount, ttl: 1 }
    }

    #[test]
    fn main_chain_ela_is_shown_but_not_offered_yet() {
        assert_eq!(
            build(&form(2, "12.5", 0, "3"), ID.into(), NOW).unwrap_err(),
            "ELA swaps on the main chain are coming: a scriptless swap, still being built."
        );
    }

    #[test]
    fn every_offered_asset_is_the_protocols_with_the_same_units() {
        let offered: Vec<_> = ASSETS.iter().filter(|a| a.chain.is_some()).collect();
        assert_eq!(offered.len(), 5);
        for asset in offered {
            let (_, _, per_coin, _) = intents_protocol::ASSETS
                .iter()
                .find(|(chain, symbol, ..)| Some(*chain) == asset.chain && *symbol == asset.symbol)
                .unwrap_or_else(|| panic!("{} is not a protocol asset", asset.symbol));
            assert_eq!(10u128.pow(asset.decimals) * asset.unit, *per_coin, "{}", asset.symbol);
        }
    }

    #[test]
    fn a_filled_form_becomes_a_valid_intent() {
        let intent = build(&form(DEFAULT_GIVE, "250", DEFAULT_WANT, "1.5"), ID.into(), NOW).unwrap();
        assert_eq!(intent.give_amount.units(), 250_000_000);
        assert_eq!(intent.min_want_amount.units(), 6 * 10u128.pow(30));
        assert_eq!(intent.expires_at, NOW + 600);
        assert!(pretty(&intent).starts_with("{\n  \"expires_at\""));
    }

    #[test]
    fn form_mistakes_read_as_sentences() {
        const USDC: usize = 3;
        const BTC: usize = 5;
        assert_eq!(build(&form(USDC, "", 0, "1"), ID.into(), NOW).unwrap_err(), "Enter both amounts.");
        assert_eq!(build(&form(USDC, "1.1234567", 0, "1"), ID.into(), NOW).unwrap_err(), "USDC has at most 6 decimals.");
        assert_eq!(build(&form(USDC, "abc", 0, "1"), ID.into(), NOW).unwrap_err(), "Amounts are numbers like 12 or 0.5.");
        assert_eq!(build(&form(USDC, "0", 0, "1"), ID.into(), NOW).unwrap_err(), "Amounts must be above zero.");
        assert_eq!(build(&form(USDC, "1", BTC, "1"), ID.into(), NOW).unwrap_err(), "One side must be NPT.");
        assert_eq!(build(&form(0, "1", 0, "1"), ID.into(), NOW).unwrap_err(), "Pick two different assets.");
    }

    #[test]
    fn random_ids_are_32_hex_characters() {
        let id = random_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(id, random_id());
    }
}
