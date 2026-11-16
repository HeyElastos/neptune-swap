//! Standing offers: a solver publishes, signed with its long-lived DID and not
//! sealed, what it will trade at what rate, on the public offers topic. A user
//! who takes one sends an [`Intent`] sealed to that solver as usual; the offer
//! binds nothing by itself, it only says where to ask.

use serde::{Deserialize, Serialize};

use crate::message::{check_expiry, check_hex, check_positive, check_version};
use crate::{Amount, Asset, Error, Intent, Message, PROTOCOL_VERSION};

/// The public topic offers travel on, per network.
pub const OFFERS_TOPIC_MAIN: &str = "neptune-swap/offers/main";
pub const OFFERS_TOPIC_TEST: &str = "neptune-swap/offers/test";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub v: u16,
    /// 16 random bytes as lowercase hex; a republished offer keeps its id.
    pub id: String,
    /// What the solver gives, and what it wants for it. Exactly one is NPT.
    pub give: Asset,
    pub want: Asset,
    /// The rate as two amounts in smallest units: for `rate_give` of `give`
    /// the solver wants `rate_want` of `want`.
    pub rate_give: Amount,
    pub rate_want: Amount,
    /// How much of `give` one swap may take.
    pub min_give: Amount,
    pub max_give: Amount,
    pub expires_at: u64,
    /// Set on the last copy of an offer its solver takes back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub withdrawn: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Message for Offer {
    const KIND: &'static str = "offer";

    fn validate(&self, now: u64) -> Result<(), Error> {
        check_version(self.v)?;
        check_hex("id", &self.id, 32)?;
        check_expiry(self.expires_at, now)?;
        self.give.validate("give")?;
        self.want.validate("want")?;
        if self.give.is_npt() == self.want.is_npt() {
            return Err(Error::invalid("give", "exactly one side must be NPT"));
        }
        check_positive("rate_give", self.rate_give)?;
        check_positive("rate_want", self.rate_want)?;
        check_positive("min_give", self.min_give)?;
        if self.min_give > self.max_give {
            return Err(Error::invalid("max_give", "below min_give"));
        }
        Ok(())
    }
}

impl Offer {
    /// What the solver wants for `give` of its asset at this rate, rounded up
    /// (the solver never gets less than its rate), or None outside the
    /// offer's limits.
    pub fn price_of(&self, give: Amount) -> Option<Amount> {
        if give < self.min_give || give > self.max_give {
            return None;
        }
        mul_div(give.units(), self.rate_want.units(), self.rate_give.units(), true).map(Amount::new)
    }

    /// What the solver gives for `want` of the asset it wants, rounded down,
    /// or None outside the offer's limits.
    pub fn amount_for(&self, want: Amount) -> Option<Amount> {
        let give = Amount::new(mul_div(want.units(), self.rate_give.units(), self.rate_want.units(), false)?);
        (give >= self.min_give && give <= self.max_give).then_some(give)
    }

    /// The intent that takes this offer: the user gives `pay` of the asset
    /// the solver wants, and asks at least what the rate gives for it.
    pub fn intent_for(&self, id: String, pay: Amount, expires_at: u64) -> Option<Intent> {
        let receive = self.amount_for(pay)?;
        Some(Intent {
            v: PROTOCOL_VERSION,
            id,
            give: self.want.clone(),
            give_amount: pay,
            want: self.give.clone(),
            min_want_amount: receive,
            expires_at,
        })
    }
}

/// floor or ceil of a * b / c without overflow (256-bit product), None if the
/// result does not fit an amount or c is 0.
fn mul_div(a: u128, b: u128, c: u128, round_up: bool) -> Option<u128> {
    if c == 0 {
        return None;
    }
    // a * b as (hi, lo) from 64-bit halves.
    let (a1, a0) = (a >> 64, a & u64::MAX as u128);
    let (b1, b0) = (b >> 64, b & u64::MAX as u128);
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let mid = (p00 >> 64) + (p01 & u64::MAX as u128) + (p10 & u64::MAX as u128);
    let lo = (p00 & u64::MAX as u128) | (mid << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    // Long division of (hi, lo) by c, bit by bit.
    let (mut q_hi, mut q_lo, mut rem) = (0u128, 0u128, 0u128);
    for i in (0..256).rev() {
        let bit = if i >= 128 { (hi >> (i - 128)) & 1 } else { (lo >> i) & 1 };
        let carry = rem >> 127;
        rem = (rem << 1) | bit;
        if carry == 1 || rem >= c {
            rem = rem.wrapping_sub(c);
            if i >= 128 {
                q_hi |= 1 << (i - 128);
            } else {
                q_lo |= 1 << i;
            }
        }
    }
    if q_hi != 0 {
        return None;
    }
    let q = if round_up && rem != 0 { q_lo.checked_add(1)? } else { q_lo };
    (q <= i128::MAX as u128).then_some(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, Chain, Net, Signed};
    use ed25519_dalek::SigningKey;

    const NOW: u64 = 1_800_000_000;
    const NPT: u128 = 4 * 10u128.pow(30);
    const USDC: u128 = 10u128.pow(6);
    const ETH: u128 = 10u128.pow(18);

    /// The solver gives NPT at 0.15 USDC each, 10 to 10,000 NPT a swap.
    fn offer() -> Offer {
        Offer {
            v: PROTOCOL_VERSION,
            id: "0123456789abcdef0123456789abcdef".into(),
            give: Asset::npt(),
            want: Asset { chain: Chain::Ethereum, symbol: "USDC".into() },
            rate_give: Amount::new(NPT),
            rate_want: Amount::new(150_000),
            min_give: Amount::new(10 * NPT),
            max_give: Amount::new(10_000 * NPT),
            expires_at: NOW + 1800,
            withdrawn: false,
        }
    }

    #[test]
    fn an_offer_signs_decodes_and_checks_its_limits() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let bytes = Signed::sign(&offer(), &key, Net::Main).unwrap().encode().unwrap();
        let decoded = decode::<Offer>(&bytes, NOW, Net::Main).unwrap();
        assert_eq!(decoded.body, offer());
        assert!(decode::<Offer>(&bytes, NOW + 1801, Net::Main).is_err(), "expired");
        assert!(decode::<Offer>(&bytes, NOW, Net::Test).is_err(), "other network");
        let both_npt = Offer { want: Asset::npt(), ..offer() };
        assert!(both_npt.validate(NOW).is_err());
        let upside_down = Offer { min_give: Amount::new(20_000 * NPT), ..offer() };
        assert!(upside_down.validate(NOW).is_err());
        let zero_rate = Offer { rate_want: Amount::new(0), ..offer() };
        assert!(zero_rate.validate(NOW).is_err());
        // A withdrawal is the same offer with the flag; the flag is absent otherwise.
        let body = String::from_utf8(crate::to_canonical_bytes(&offer()).unwrap()).unwrap();
        assert!(!body.contains("withdrawn"));
        let gone = Offer { withdrawn: true, ..offer() };
        assert!(gone.validate(NOW).is_ok());
    }

    #[test]
    fn rates_round_in_the_solvers_favour_without_overflow() {
        let o = offer();
        // 1500 NPT cost 225 USDC; 100 USDC buy 666.66… NPT, rounded down.
        assert_eq!(o.price_of(Amount::new(1500 * NPT)), Some(Amount::new(225 * USDC)));
        let got = o.amount_for(Amount::new(100 * USDC)).unwrap().units();
        assert_eq!(got, 2_666_666_666_666_666_666_666_666_666_666_666, "666.66… NPT, floor");
        assert!(o.price_of(Amount::new(NPT)).is_none(), "below the minimum");
        assert!(o.amount_for(Amount::new(2_000 * USDC)).is_none(), "above the maximum");
        // NPT for ETH: products far beyond 128 bits.
        let eth = Offer {
            want: Asset { chain: Chain::Ethereum, symbol: "ETH".into() },
            rate_give: Amount::new(12_000 * NPT),
            rate_want: Amount::new(ETH),
            max_give: Amount::new(1_000_000 * NPT),
            ..offer()
        };
        assert_eq!(eth.amount_for(Amount::new(ETH / 2)), Some(Amount::new(6_000 * NPT)));
        assert_eq!(eth.price_of(Amount::new(6_000 * NPT)), Some(Amount::new(ETH / 2)));
        assert_eq!(mul_div(7, 1, 2, true), Some(4));
        assert_eq!(mul_div(7, 1, 2, false), Some(3));
        assert_eq!(mul_div(u128::MAX, u128::MAX, u128::MAX, false), None, "beyond an amount");
        assert_eq!(mul_div(i128::MAX as u128, 3, 3, false), Some(i128::MAX as u128));
        assert_eq!(mul_div(1, 1, 0, false), None);
    }

    #[test]
    fn taking_an_offer_makes_a_valid_intent() {
        let i = offer().intent_for("ffeeddccbbaa99887766554433221100".into(), Amount::new(150 * USDC), NOW + 600).unwrap();
        assert_eq!((i.give.symbol.as_str(), i.want.symbol.as_str()), ("USDC", "NPT"));
        assert_eq!(i.min_want_amount, Amount::new(1000 * NPT));
        assert!(i.validate(NOW).is_ok());
    }
}
