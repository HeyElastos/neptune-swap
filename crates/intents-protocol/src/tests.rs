use ed25519_dalek::SigningKey;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::*;

const NOW: u64 = 1_800_000_000;
const NET: Net = Net::Main;
const H: u64 = 3600;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn evm(seed: char) -> String {
    seed.to_string().repeat(40)
}

fn npt_key(seed: char) -> String {
    seed.to_string().repeat(80)
}

fn hash_lock() -> String {
    hex::encode(Sha256::digest([7u8; 32]))
}

/// The user gives 1000 nau of NPT for at least 500 USDC units.
fn intent() -> Intent {
    Intent {
        v: PROTOCOL_VERSION,
        id: "00112233445566778899aabbccddeeff".into(),
        give: Asset::npt(),
        give_amount: Amount::new(1_000),
        want: Asset { chain: Chain::Ethereum, symbol: "USDC".into() },
        min_want_amount: Amount::new(500),
        expires_at: NOW + 600,
    }
}

/// The same trade the other way round: the user gives USDC and wants NPT.
fn intent_for_npt() -> Intent {
    Intent {
        give: Asset { chain: Chain::Ethereum, symbol: "USDC".into() },
        give_amount: Amount::new(500),
        want: Asset::npt(),
        min_want_amount: Amount::new(1_000),
        ..intent()
    }
}

fn signed<T: Message>(body: &T, seed: u8) -> Vec<u8> {
    Signed::sign(body, &key(seed), NET).unwrap().encode().unwrap()
}

fn decoded<T: Message>(body: &T, seed: u8) -> Decoded<T> {
    decode(&signed(body, seed), NOW, NET).unwrap()
}

/// A quote for `intent` (user signs with seed 1), as the solver (seed 2) gives it.
fn quote_for(intent: &Intent) -> Quote {
    let (claim, refund) = if intent.give.is_npt() { (npt_key('a'), evm('b')) } else { (evm('a'), npt_key('b')) };
    Quote {
        v: PROTOCOL_VERSION,
        intent: decoded(intent, 1).digest,
        want_amount: Amount::new(1_000),
        other_lock_secs: 2 * H,
        neptune_lock_secs: 2 * H + Chain::Ethereum.gap_secs() + NEPTUNE_SETTLE_SECS,
        hash_lock: intent.want.is_npt().then(hash_lock),
        claim_key: claim,
        refund_key: refund,
        expires_at: NOW + 120,
    }
}

fn accept_for(intent: &Intent, quote: &Quote) -> Accept {
    let (claim, refund) = if intent.give.is_npt() { (evm('c'), npt_key('d')) } else { (npt_key('c'), evm('d')) };
    Accept {
        v: PROTOCOL_VERSION,
        quote: decoded(quote, 2).digest,
        hash_lock: intent.give.is_npt().then(hash_lock),
        claim_key: claim,
        refund_key: refund,
    }
}

/// Checks a whole intent → quote → accept round as the solver would.
fn check(intent: &Intent, quote: &Quote, accept: &Accept, accept_seed: u8, now: u64) -> Result<(), Error> {
    let i = decoded(intent, 1);
    let q = decoded(quote, 2);
    let a = decoded(accept, accept_seed);
    a.body.answers(&a.signer, &i, &q, now)
}

#[test]
fn did_key_round_trips_with_the_ed25519_prefix() {
    let did = DidKey::new(key(1).verifying_key());
    let text = did.to_string();
    assert!(text.starts_with("did:key:z6Mk"), "{text}");
    assert_eq!(DidKey::parse(&text).unwrap(), did);
    assert!(DidKey::parse("did:web:example.com").is_err());
}

#[test]
fn canonical_bytes_sort_keys_at_every_level() {
    let value = json!({"b": 1, "a": {"d": [2, {"z": 0, "y": 1}], "c": "x"}});
    let bytes = to_canonical_bytes(&value).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), r#"{"a":{"c":"x","d":[2,{"y":1,"z":0}]},"b":1}"#);
}

#[test]
fn a_signed_intent_decodes_to_its_signer_and_digest() {
    let d = decoded(&intent(), 1);
    assert_eq!(d.body, intent());
    assert_eq!(d.signer, DidKey::new(key(1).verifying_key()));
    assert_eq!(d.digest.len(), 64);
    assert_ne!(d.digest, decoded(&intent(), 3).digest, "the digest covers the signer");
}

#[test]
fn a_testnet_message_does_not_verify_on_mainnet() {
    let test = Signed::sign(&intent(), &key(1), Net::Test).unwrap().encode().unwrap();
    assert_eq!(decode::<Intent>(&test, NOW, Net::Main), Err(Error::BadSignature));
    assert!(decode::<Intent>(&test, NOW, Net::Test).is_ok());
}

#[test]
fn a_changed_body_or_signer_breaks_the_signature() {
    let mut envelope = Signed::sign(&intent(), &key(1), NET).unwrap();
    let original = envelope.clone();
    envelope.body = envelope.body.replace("\"500\"", "\"5\"");
    assert_ne!(envelope, original);
    assert_eq!(decode::<Intent>(&envelope.encode().unwrap(), NOW, NET), Err(Error::BadSignature));
    let mut swapped = original;
    swapped.signer = DidKey::new(key(2).verifying_key()).to_string();
    assert_eq!(decode::<Intent>(&swapped.encode().unwrap(), NOW, NET), Err(Error::BadSignature));
}

/// A body with a duplicate key verifies only if it was signed that way, and then it is
/// refused: one reader could take the first value and another the last.
#[test]
fn a_body_that_is_not_canonical_is_refused_even_when_signed() {
    let canonical = String::from_utf8(to_canonical_bytes(&intent()).unwrap()).unwrap();
    for body in [
        canonical.replacen("{", "{\"give_amount\":\"1\",", 1),
        canonical.replace(",", ", "),
        canonical.replace("\"USDC\"", "\"\\u0055SDC\""),
    ] {
        let signer = DidKey::new(key(1).verifying_key()).to_string();
        let bytes = [b"neptune-intents/v2/main\n".as_slice(), b"intent\n", signer.as_bytes(), b"\n", body.as_bytes()].concat();
        let sig = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, ed25519_dalek::Signer::sign(&key(1), &bytes).to_bytes());
        let envelope = Signed { kind: "intent".into(), signer, body, sig };
        assert!(matches!(decode::<Intent>(&envelope.encode().unwrap(), NOW, NET), Err(Error::Malformed(_))));
    }
}

#[test]
fn kinds_sizes_and_unknown_fields_are_checked() {
    let q = quote_for(&intent());
    assert!(matches!(decode::<Intent>(&signed(&q, 2), NOW, NET), Err(Error::WrongKind { expected: "intent", .. })));
    assert_eq!(decode::<Intent>(&vec![b' '; MAX_MESSAGE_BYTES + 1], NOW, NET), Err(Error::TooLarge(MAX_MESSAGE_BYTES + 1)));
    let mut value: serde_json::Value = serde_json::from_slice(&signed(&intent(), 1)).unwrap();
    value["extra"] = json!(1);
    assert!(matches!(decode::<Intent>(value.to_string().as_bytes(), NOW, NET), Err(Error::Malformed(_))));
}

#[test]
fn expiry_is_live_and_bounded() {
    assert_eq!(decode::<Intent>(&signed(&intent(), 1), NOW + 600, NET), Err(Error::Expired(NOW + 600)));
    let far = Intent { expires_at: NOW + MAX_TTL_SECS + 1, ..intent() };
    assert_eq!(far.validate(NOW), Err(Error::invalid("expires_at", "too far ahead")));
}

#[test]
fn only_listed_assets_with_exactly_one_npt_side() {
    let usdc_on_bitcoin = Intent { want: Asset { chain: Chain::Bitcoin, symbol: "USDC".into() }, ..intent() };
    assert_eq!(usdc_on_bitcoin.validate(NOW), Err(Error::invalid("want", "not a listed asset")));
    let both = Intent { want: Asset::npt(), ..intent() };
    assert!(both.validate(NOW).is_err());
    let neither = Intent { give: Asset { chain: Chain::Bitcoin, symbol: "BTC".into() }, ..intent() };
    assert!(neither.validate(NOW).is_err());
}

#[test]
fn amounts_are_canonical_positive_and_fit_neptune() {
    for bad in ["007", "-1", "1.0", " 1", "", "170141183460469231731687303715884105728"] {
        assert!(Amount::try_from(bad.to_string()).is_err(), "{bad:?}");
    }
    assert!(Amount::try_from(i128::MAX.to_string()).is_ok());
    let zero = Intent { give_amount: Amount::new(0), ..intent() };
    assert_eq!(zero.validate(NOW), Err(Error::invalid("give_amount", "must be positive")));
}

#[test]
fn a_full_round_checks_in_both_directions() {
    for i in [intent(), intent_for_npt()] {
        let q = quote_for(&i);
        assert_eq!(check(&i, &q, &accept_for(&i, &q), 1, NOW), Ok(()));
    }
}

#[test]
fn the_npt_giver_and_only_it_sets_the_hash_lock() {
    let i = intent();
    let q = Quote { hash_lock: Some(hash_lock()), ..quote_for(&i) };
    assert_eq!(q.answers(&decoded(&i, 1)), Err(Error::invalid("hash_lock", "only the NPT giver sets it")));
    let q = quote_for(&i);
    let a = Accept { hash_lock: None, ..accept_for(&i, &q) };
    assert_eq!(check(&i, &q, &a, 1, NOW), Err(Error::invalid("hash_lock", "missing: the NPT giver sets it")));
}

/// A second intent reusing the first one's id does not match a quote made for the first.
#[test]
fn a_quote_is_bound_to_its_intent_not_to_an_id() {
    let q = quote_for(&intent());
    let cheaper = Intent { give_amount: Amount::new(1), ..intent() };
    assert_eq!(q.answers(&decoded(&cheaper, 1)), Err(Error::invalid("intent", "does not match the intent")));
    assert_eq!(q.answers(&decoded(&intent(), 3)), Err(Error::invalid("intent", "does not match the intent")));
}

#[test]
fn an_accept_comes_from_the_intent_signer_while_the_quote_stands() {
    let i = intent();
    let q = quote_for(&i);
    let a = accept_for(&i, &q);
    assert_eq!(check(&i, &q, &a, 3, NOW), Err(Error::invalid("signer", "not the intent's signer")));
    assert_eq!(check(&i, &q, &a, 1, q.expires_at), Err(Error::Expired(q.expires_at)));
    let other = Quote { want_amount: Amount::new(2_000), ..q.clone() };
    assert_eq!(check(&i, &other, &a, 1, NOW), Err(Error::invalid("quote", "does not match the quote")));
}

#[test]
fn keys_must_fit_their_chains() {
    let i = intent();
    let q = Quote { claim_key: evm('a'), ..quote_for(&i) };
    assert_eq!(q.answers(&decoded(&i, 1)), Err(Error::invalid("claim_key", "wrong length or not lowercase hex")));
}

#[test]
fn timeouts_fit_the_other_chain_and_stay_bounded() {
    let i = decoded(&intent(), 1);
    let q = quote_for(&intent());
    let short = Quote { other_lock_secs: Chain::Ethereum.min_lock_secs() - 1, ..q.clone() };
    assert!(matches!(short.answers(&i), Err(Error::Invalid { field: "other_lock_secs", .. })));
    let tight = Quote { neptune_lock_secs: q.neptune_lock_secs - 1, ..q.clone() };
    assert!(matches!(tight.answers(&i), Err(Error::Invalid { field: "neptune_lock_secs", .. })));
    let long = Quote { other_lock_secs: 40 * H, neptune_lock_secs: MAX_LOCK_SECS + 1, ..q.clone() };
    assert!(matches!(long.answers(&i), Err(Error::Invalid { field: "neptune_lock_secs", .. })));
    let huge = Quote { other_lock_secs: u64::MAX, ..q };
    assert!(huge.answers(&i).is_err());
}

/// The receiver's lock refunds a full gap before the Neptune lock as it is on chain,
/// however late the giver locked; too late, and it locks nothing.
#[test]
fn the_other_refund_follows_the_neptune_lock_on_chain() {
    let eth = Chain::Ethereum;
    let neptune_refund_at = NOW + 7 * H;
    let at = other_refund_at(eth, neptune_refund_at, 2 * H, NOW).unwrap();
    assert_eq!(at, NOW + 2 * H);
    let late = NOW + 2 * H;
    let at = other_refund_at(eth, neptune_refund_at, 2 * H, late).unwrap();
    assert_eq!(at, neptune_refund_at - eth.gap_secs());
    assert!(other_refund_at(eth, neptune_refund_at, 2 * H, late + 1).is_err());
    assert!(other_refund_at(Chain::Bitcoin, neptune_refund_at, 6 * H, NOW).is_err());
}

#[test]
fn the_giver_reveals_only_with_the_claim_margin_left() {
    let at = NOW + 2 * H;
    assert!(may_reveal(Chain::Ethereum, at, at - H));
    assert!(!may_reveal(Chain::Ethereum, at, at - H + 1));
    assert!(!may_reveal(Chain::Bitcoin, at, at - H));
}

/// Every chain must hash the same 32 bytes the same way: one shared vector.
#[test]
fn the_hash_lock_vector_is_sha256_of_32_raw_bytes() {
    assert_eq!(hex::encode(Sha256::digest([0u8; 32])), "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925");
}

#[test]
fn typed_amounts_convert_exactly_to_smallest_units() {
    let npt = |s: &str| Amount::parse_decimal(s, 30, 4).map(Amount::units);
    assert_eq!(npt("1.5"), Ok(6 * 10u128.pow(30)));
    assert_eq!(npt("0.000000000000000000000000000001"), Ok(4));
    for bad in ["", ".5", "1.", "1,5", "-1", "1e3", "0.1234567"] {
        assert!(Amount::parse_decimal(bad, 6, 1).is_err(), "{bad:?} was accepted");
    }
}

#[test]
fn lock_messages_belong_to_one_deal_and_come_from_the_right_side() {
    // The user (key 1) gives NPT; the solver (key 2) quotes.
    let i = intent();
    let q = quote_for(&i);
    let a = accept_for(&i, &q);
    let (quote, accept) = (decoded(&q, 2), decoded(&a, 1));
    let note = serde_json::json!({ "hash_lock": hash_lock(), "amount": "2" });

    let npt = NptLocked { v: PROTOCOL_VERSION, deal: accept.digest.clone(), note: note.clone(), from_height: 7 };
    let got = decoded(&npt, 1);
    got.body.answers(&got.signer, &accept, &accept.signer).unwrap();
    let from_solver = decoded(&npt, 2);
    assert!(from_solver.body.answers(&from_solver.signer, &accept, &accept.signer).is_err(), "only the NPT giver");
    let other = NptLocked { deal: quote.digest.clone(), ..npt.clone() };
    let got = decoded(&other, 1);
    assert!(got.body.answers(&got.signer, &accept, &accept.signer).is_err(), "another deal");
    let not_object = NptLocked { note: serde_json::json!("x"), ..npt };
    assert!(decode::<NptLocked>(&signed(&not_object, 1), NOW, Net::Test).is_err());

    let evm = EvmLocked { v: PROTOCOL_VERSION, deal: accept.digest.clone(), contract: "ab".repeat(20), id: "cd".repeat(32) };
    let got = decoded(&evm, 2);
    got.body.answers(&got.signer, &accept, &quote).unwrap();
    let from_user = decoded(&evm, 1);
    assert!(from_user.body.answers(&from_user.signer, &accept, &quote).is_err(), "only the quoting solver");
    // When the solver gives NPT (its quote carries the hash lock), the user locks on the
    // EVM chain and only the user's notice counts.
    let solver_gives_npt = Decoded { body: Quote { hash_lock: Some(hash_lock()), ..quote.body.clone() }, ..quote.clone() };
    from_user.body.answers(&from_user.signer, &accept, &solver_gives_npt).unwrap();
    let got = decoded(&evm, 2);
    assert!(got.body.answers(&got.signer, &accept, &solver_gives_npt).is_err(), "only the user then");
    let bad = EvmLocked { contract: "0x".to_string() + &"ab".repeat(19), ..evm };
    assert!(decode::<EvmLocked>(&signed(&bad, 2), NOW, Net::Test).is_err(), "contract without 0x, 40 hex");
}

#[test]
fn eth_on_base_is_its_own_asset() {
    let base = Intent { want: Asset { chain: Chain::Base, symbol: "USDC".into() }, ..intent() };
    assert!(base.validate(NOW).is_ok());
    let ela = Intent { want: Asset { chain: Chain::Base, symbol: "ELA".into() }, ..intent() };
    assert!(ela.validate(NOW).is_err());
    assert_eq!(asset_row(Chain::Base, "USDC"), Some((1_000_000, Some("833589fcd6edb6e08f4c7c32d4f71b54bda02913"))));
    assert_ne!(asset_row(Chain::Base, "USDC"), asset_row(Chain::Ethereum, "USDC"));
    assert_eq!((Chain::Base.evm_chain_id(), Chain::Neptune.evm_chain_id()), (Some(8453), None));
    // ELA trades on the Elastos main chain only (a scriptless swap), never on an EVM chain.
    assert!(ASSETS.iter().all(|(chain, symbol, ..)| *symbol != "ELA" || !chain.is_evm()));
    assert!(serde_json::from_str::<Chain>("\"esc\"").is_err(), "Elastos ESC is not a chain of this protocol");
}

#[test]
fn a_decoded_message_is_kept_with_its_signer_as_text() {
    let d = decoded(&intent(), 1);
    let mut record = serde_json::to_value(&d).unwrap();
    assert_eq!(record["signer"], d.signer.to_string());
    assert_eq!(serde_json::from_value::<Decoded<Intent>>(record.clone()).unwrap(), d);
    record["signer"] = json!("did:key:z6Mk");
    assert!(serde_json::from_value::<Decoded<Intent>>(record).is_err(), "not a did:key");
}

#[test]
fn ethereum_waits_past_finality_and_base_twenty_blocks() {
    assert!(Chain::Ethereum.confirmations() > 95, "past the 64 to 95 slots to finality");
    assert_eq!(Chain::Base.confirmations(), 20);
}
