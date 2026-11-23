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

/// The deterministic deployment proxy (the same address on Base, Ethereum and a local
/// anvil chain): `CREATE2` from it with a salt puts code at an address that depends only on
/// that salt and the creation code.
pub const CREATE2_DEPLOYER: Address = [
    0x4e, 0x59, 0xb4, 0x48, 0x47, 0xb3, 0x79, 0x57, 0x85, 0x88, 0x92, 0x0c, 0xa7, 0x8f, 0xbf, 0x26, 0xc0, 0xb4, 0x95, 0x6c,
];
pub const SALT: Word = [0; 32];

/// keccak256 of `NeptuneHtlc`'s creation code (`NeptuneHtlc.init.hex`), built from
/// `contracts/htlc-evm` with its `foundry.toml` (no metadata, so the build is reproducible).
pub const INIT_CODE_KECCAK: Word = [
    0x10, 0x8d, 0x71, 0x5f, 0x9d, 0xe4, 0xd7, 0x38, 0x59, 0x6f, 0xb1, 0xaf, 0x7e, 0x68, 0xc4, 0xb9,
    0x1d, 0xba, 0xf4, 0x7d, 0xef, 0xf2, 0x24, 0x49, 0xf7, 0xee, 0x5d, 0x69, 0xc9, 0xca, 0x7c, 0xef,
];

/// keccak256 of the code that creation code leaves deployed (`NeptuneHtlc.runtime.hex`).
pub const RUNTIME_CODE_KECCAK: Word = [
    0x7c, 0x5c, 0x8e, 0xf2, 0xa5, 0x6d, 0xda, 0x8d, 0x7f, 0x48, 0x87, 0x51, 0x15, 0x97, 0x17, 0x96,
    0xaf, 0xf7, 0x80, 0x8f, 0x77, 0xc8, 0x60, 0x98, 0xa4, 0xd5, 0x62, 0xde, 0x57, 0x0c, 0xaa, 0x49,
];

/// **The** swap contract, on every chain: `CREATE2` from [`CREATE2_DEPLOYER`] with [`SALT`]
/// and the creation code above. Anyone may deploy it (the result is the same), and nobody can
/// put other code or pre-filled storage there: the address commits to the creation code,
/// which starts with empty storage. A lock counts only at this address. Matching the code
/// alone would not do: other creation code can write fake locks into storage and then leave
/// the same runtime code behind.
pub const HTLC_ADDRESS: Address = [
    0x5d, 0xc5, 0x93, 0x0d, 0xab, 0x83, 0x32, 0x9a, 0x3d, 0xd8, 0xe8, 0x5e, 0xe1, 0xde, 0x7e, 0x83, 0x06, 0xd6, 0xb2, 0x60,
];

/// The address `CREATE2` gives creation code with this hash.
pub fn create2_address(deployer: &Address, salt: &Word, init_code_keccak: &Word) -> Address {
    let mut bytes = vec![0xff];
    bytes.extend_from_slice(deployer);
    bytes.extend_from_slice(salt);
    bytes.extend_from_slice(init_code_keccak);
    keccak(&bytes)[12..].try_into().unwrap()
}

/// Whether `code` (as `eth_getCode` returns it at [`HTLC_ADDRESS`]) is the deployed contract:
/// a check that it has been deployed on this chain, not a substitute for the address.
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
/// `chainTime()`: the chain's clock as the contract sees it (`block.timestamp`).
pub fn chain_time() -> Vec<u8> {
    call("chainTime()", &[])
}

/// The answer to `chainTime()`.
pub fn decode_chain_time(ret: &[u8]) -> Option<u64> {
    let w: &[u8; 32] = ret.try_into().ok()?;
    w[..24].iter().all(|b| *b == 0).then(|| u64::from_be_bytes(w[24..].try_into().unwrap()))
}

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
        assert_eq!(hex(&chain_time()), "0xd7a9ea8d");
        let mut ret = [0u8; 32];
        ret[24..].copy_from_slice(&1_900_000_123u64.to_be_bytes());
        assert_eq!(decode_chain_time(&ret), Some(1_900_000_123));
        assert_eq!(decode_chain_time(&ret[1..]), None);
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
    fn the_pinned_address_and_code_are_the_checked_in_build() {
        let init = parse_hex(include_str!("../../../contracts/htlc-evm/NeptuneHtlc.init.hex")).unwrap();
        assert_eq!(keccak(&init), INIT_CODE_KECCAK);
        assert_eq!(create2_address(&CREATE2_DEPLOYER, &SALT, &INIT_CODE_KECCAK), HTLC_ADDRESS);
        assert_eq!(hex(&HTLC_ADDRESS), "0x5dc5930dab83329a3dd8e85ee1de7e8306d6b260");
        let code = parse_hex(include_str!("../../../contracts/htlc-evm/NeptuneHtlc.runtime.hex")).unwrap();
        assert!(is_htlc_code(&code));
        assert!(!is_htlc_code(&code[1..]));
        // Shanghai code (foundry.toml): no TLOAD, TSTORE, MCOPY, BLOBHASH or BLOBBASEFEE, which
        // a chain without Cancun would reject at claim or refund time.
        let mut i = 0;
        while i < code.len() {
            assert!(![0x5c, 0x5d, 0x5e, 0x49, 0x4a].contains(&code[i]), "Cancun opcode {:#04x} at {i}", code[i]);
            i += 1 + if (0x60..=0x7f).contains(&code[i]) { usize::from(code[i] - 0x5f) } else { 0 };
        }
        // When Foundry has built the contract here, the build must match the checked-in code.
        let artifact = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contracts/htlc-evm/out/NeptuneHtlc.sol/NeptuneHtlc.json");
        if let Ok(json) = std::fs::read_to_string(artifact) {
            let at = json.find("\"deployedBytecode\"").unwrap();
            let start = json[at..].find("\"object\":\"").unwrap() + at + 10;
            let end = json[start..].find('"').unwrap() + start;
            assert_eq!(parse_hex(&json[start..end]).unwrap(), code, "rebuild changed the code: update the pins");
            let at = json.find("\"bytecode\"").unwrap();
            let start = json[at..].find("\"object\":\"").unwrap() + at + 10;
            let end = json[start..].find('"').unwrap() + start;
            assert_eq!(parse_hex(&json[start..end]).unwrap(), init, "rebuild changed the creation code: update the pins");
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
