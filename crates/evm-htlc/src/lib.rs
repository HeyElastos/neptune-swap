//! Calldata and logs for `contracts/htlc-evm` (`NeptuneHtlc`), the EVM half of a
//! Neptune swap. The capsule hands this calldata to elastOS (chain-provider
//! `prepare_transaction`, then a Wallet approval); this crate signs nothing.
//!
//! Every argument of these functions is a static ABI type, so each call is the
//! 4-byte selector followed by one 32-byte word per argument.

#![forbid(unsafe_code)]

use sha2::Digest as _;
use sha3::Keccak256;

pub type Address = [u8; 20];
pub type Word = [u8; 32];

/// keccak256 of `NeptuneHtlc`'s deployed code, built from `contracts/htlc-evm` with its
/// `foundry.toml` (no metadata, so the build is reproducible). A lock counts only in a
/// contract whose code (`eth_getCode`) has this hash: the contract is recognized by what it
/// does, never by its address or by who deployed it, so no registry or deployer is trusted.
pub const RUNTIME_CODE_KECCAK: Word = [
    0xda, 0x6b, 0x4d, 0x8a, 0x99, 0xe7, 0x06, 0x5d, 0xb7, 0x72, 0xff, 0xee, 0x9a, 0x41, 0x06, 0x3b,
    0x6c, 0x65, 0x0e, 0x37, 0xa5, 0x53, 0x7d, 0xcb, 0x19, 0xc2, 0xc9, 0xa4, 0xfb, 0x20, 0xfa, 0x3b,
];

/// Whether `code` (as `eth_getCode` returns it) is `NeptuneHtlc`.
pub fn is_htlc_code(code: &[u8]) -> bool {
    keccak(code) == RUNTIME_CODE_KECCAK
}

/// The secret's hash lock: SHA-256 of the 32 bytes, as on the Neptune side.
pub fn hash_lock(secret: &Word) -> Word {
    sha2::Sha256::digest(secret).into()
}

pub fn keccak(bytes: &[u8]) -> Word {
    Keccak256::digest(bytes).into()
}

fn selector(signature: &str) -> [u8; 4] {
    keccak(signature.as_bytes())[..4].try_into().unwrap()
}

fn address_word(a: &Address) -> Word {
    let mut w = [0; 32];
    w[12..].copy_from_slice(a);
    w
}

fn uint_word(n: u128) -> Word {
    let mut w = [0; 32];
    w[16..].copy_from_slice(&n.to_be_bytes());
    w
}

fn call(signature: &str, words: &[Word]) -> Vec<u8> {
    let mut data = selector(signature).to_vec();
    for w in words {
        data.extend_from_slice(w);
    }
    data
}

/// `lock(receiver, hashLock, refundAfter)`, sent with the ETH as value.
pub fn lock(receiver: &Address, hash_lock: &Word, refund_after: u64) -> Vec<u8> {
    call("lock(address,bytes32,uint64)", &[address_word(receiver), *hash_lock, uint_word(refund_after.into())])
}

/// `lockToken(token, amount, receiver, hashLock, refundAfter)`, after `approve`.
pub fn lock_token(token: &Address, amount: u128, receiver: &Address, hash_lock: &Word, refund_after: u64) -> Vec<u8> {
    call(
        "lockToken(address,uint256,address,bytes32,uint64)",
        &[address_word(token), uint_word(amount), address_word(receiver), *hash_lock, uint_word(refund_after.into())],
    )
}

/// ERC-20 `approve(spender, amount)`, so the contract may take the tokens.
pub fn approve(spender: &Address, amount: u128) -> Vec<u8> {
    call("approve(address,uint256)", &[address_word(spender), uint_word(amount)])
}

pub fn claim(id: &Word, preimage: &Word) -> Vec<u8> {
    call("claim(bytes32,bytes32)", &[*id, *preimage])
}

pub fn refund(id: &Word) -> Vec<u8> {
    call("refund(bytes32)", &[*id])
}

/// `locks(id)`: the public getter of a lock's terms and state.
pub fn locks(id: &Word) -> Vec<u8> {
    call("locks(bytes32)", &[*id])
}

/// A lock as `locks(id)` returns it. An unknown id comes back all zero
/// (`sender` zero).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockView {
    pub sender: Address,
    pub receiver: Address,
    pub token: Address,
    pub amount: u128,
    pub hash_lock: Word,
    pub refund_after: u64,
    pub spent: bool,
}

pub fn decode_lock(ret: &[u8]) -> Option<LockView> {
    if ret.len() != 7 * 32 {
        return None;
    }
    let word = |i: usize| -> &[u8] { &ret[i * 32..(i + 1) * 32] };
    let address = |i: usize| -> Option<Address> {
        let w = word(i);
        w[..12].iter().all(|b| *b == 0).then(|| w[12..].try_into().unwrap())
    };
    let small = |i: usize, bytes: usize| -> Option<&[u8]> {
        let w = word(i);
        w[..32 - bytes].iter().all(|b| *b == 0).then(|| &w[32 - bytes..])
    };
    Some(LockView {
        sender: address(0)?,
        receiver: address(1)?,
        token: address(2)?,
        amount: u128::from_be_bytes(small(3, 16)?.try_into().ok()?),
        hash_lock: word(4).try_into().ok()?,
        refund_after: u64::from_be_bytes(small(5, 8)?.try_into().ok()?),
        spent: match small(6, 1)? {
            [0] => false,
            [1] => true,
            _ => return None,
        },
    })
}

/// The id the contract gives a lock with these terms (`lockId`).
pub fn lock_id(sender: &Address, receiver: &Address, token: &Address, amount: u128, hash_lock: &Word, refund_after: u64) -> Word {
    let words = [
        address_word(sender),
        address_word(receiver),
        address_word(token),
        uint_word(amount),
        *hash_lock,
        uint_word(refund_after.into()),
    ];
    keccak(&words.concat())
}

/// A log as a receipt carries it.
pub struct Log<'a> {
    pub address: &'a Address,
    pub topics: &'a [Word],
    pub data: &'a [u8],
}

/// The secret a `Claimed(id, preimage)` log of `contract` published for `id`,
/// checked against the hash lock it must open.
pub fn claimed_secret(log: &Log, contract: &Address, id: &Word, expected_hash_lock: &Word) -> Option<Word> {
    if log.address != contract || log.topics.len() != 2 || log.topics[0] != keccak(b"Claimed(bytes32,bytes32)") {
        return None;
    }
    if &log.topics[1] != id || log.data.len() != 32 {
        return None;
    }
    let preimage: Word = log.data.try_into().ok()?;
    (hash_lock(&preimage) == *expected_hash_lock).then_some(preimage)
}

pub fn parse_address(text: &str) -> Option<Address> {
    let bytes = parse_hex(text)?;
    bytes.try_into().ok()
}

pub fn parse_word(text: &str) -> Option<Word> {
    let bytes = parse_hex(text)?;
    bytes.try_into().ok()
}

pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let t = text.trim().strip_prefix("0x").unwrap_or(text.trim());
    if t.len() % 2 != 0 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok()).collect()
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::from("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // From `cast sig` / `cast keccak` (Foundry 1.9.0) against the compiled contract's ABI.
    #[test]
    fn selectors_and_topics_match_the_contract() {
        assert_eq!(hex(&selector("lock(address,bytes32,uint64)")), "0x473500ff");
        assert_eq!(hex(&selector("lockToken(address,uint256,address,bytes32,uint64)")), "0x9e8ee5ca");
        assert_eq!(hex(&selector("claim(bytes32,bytes32)")), "0x84cc9dfb");
        assert_eq!(hex(&selector("refund(bytes32)")), "0x7249fbb6");
        assert_eq!(hex(&selector("approve(address,uint256)")), "0x095ea7b3");
        assert_eq!(
            hex(&keccak(b"Claimed(bytes32,bytes32)")),
            "0x38d6042dbdae8e73a7f6afbabd3fbe0873f9f5ed3cd71294591c3908c2e65fee"
        );
    }

    #[test]
    fn calls_are_a_selector_and_one_word_per_argument() {
        let receiver = [0xbb; 20];
        let data = lock(&receiver, &[7; 32], 1_800_003_600);
        assert_eq!(data.len(), 4 + 3 * 32);
        assert_eq!(&data[4 + 12..4 + 32], &receiver);
        assert_eq!(u64::from_be_bytes(data[4 + 64 + 24..].try_into().unwrap()), 1_800_003_600);
        assert_eq!(lock_token(&[1; 20], 250_000_000, &receiver, &[7; 32], 1).len(), 4 + 5 * 32);
        assert_eq!(claim(&[1; 32], &[2; 32]).len(), 4 + 64);
        assert_eq!(refund(&[1; 32]).len(), 4 + 32);
    }

    #[test]
    fn the_hash_lock_matches_the_neptune_side_vector() {
        assert_eq!(hex(&hash_lock(&[0; 32])), "0x66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925");
    }

    #[test]
    fn a_claimed_log_yields_the_secret_only_when_it_is_the_one() {
        let contract = [0xcc; 20];
        let id = [9; 32];
        let secret = [3; 32];
        let lock = hash_lock(&secret);
        let topics = [keccak(b"Claimed(bytes32,bytes32)"), id];
        let log = Log { address: &contract, topics: &topics, data: &secret };
        assert_eq!(claimed_secret(&log, &contract, &id, &lock), Some(secret));
        assert_eq!(claimed_secret(&log, &[0xdd; 20], &id, &lock), None, "another contract");
        assert_eq!(claimed_secret(&log, &contract, &[8; 32], &lock), None, "another lock");
        assert_eq!(claimed_secret(&log, &contract, &id, &[0; 32]), None, "does not open the hash lock");
    }

    #[test]
    fn a_lock_view_decodes_and_refuses_odd_words() {
        let mut ret = vec![0u8; 7 * 32];
        ret[12..32].copy_from_slice(&[0xaa; 20]);
        ret[32 + 12..64].copy_from_slice(&[0xbb; 20]);
        ret[3 * 32 + 31] = 5;
        ret[4 * 32..5 * 32].copy_from_slice(&[7; 32]);
        ret[5 * 32 + 31] = 9;
        ret[6 * 32 + 31] = 1;
        let v = decode_lock(&ret).unwrap();
        assert_eq!((v.sender, v.receiver, v.token), ([0xaa; 20], [0xbb; 20], [0; 20]));
        assert_eq!((v.amount, v.hash_lock, v.refund_after, v.spent), (5, [7; 32], 9, true));
        ret[6 * 32 + 31] = 2;
        assert!(decode_lock(&ret).is_none(), "a bool is 0 or 1");
        assert!(decode_lock(&ret[..32]).is_none());
        assert_eq!(hex(&locks(&[0; 32])[..4]), "0xed5e8437"); // cast sig "locks(bytes32)"
    }

    #[test]
    fn the_pinned_code_hash_is_the_checked_in_build() {
        let code = parse_hex(include_str!("../../../contracts/htlc-evm/NeptuneHtlc.runtime.hex")).unwrap();
        assert!(is_htlc_code(&code));
        assert!(!is_htlc_code(&code[1..]));
        // When Foundry has built the contract here, the build must match the checked-in code.
        let artifact = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contracts/htlc-evm/out/NeptuneHtlc.sol/NeptuneHtlc.json");
        if let Ok(json) = std::fs::read_to_string(artifact) {
            let at = json.find("\"deployedBytecode\"").unwrap();
            let start = json[at..].find("\"object\":\"").unwrap() + at + 10;
            let end = json[start..].find('"').unwrap() + start;
            assert_eq!(parse_hex(&json[start..end]).unwrap(), code, "rebuild changed the code: update the pin");
        }
    }

    #[test]
    fn hex_round_trips() {
        let a = parse_address("0x00000000000000000000000000000000000B0B00").unwrap();
        assert_eq!(hex(&a), "0x00000000000000000000000000000000000b0b00");
        assert!(parse_address("0x1234").is_none());
        assert!(parse_word("zz").is_none());
    }
}
