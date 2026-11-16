//! Prints NeptuneHtlc calldata and ids from this crate, for driving a local
//! chain with `cast` (see PLAN.md, M8):
//!   calldata lock <receiver> <hash_lock> <refund_after>
//!   calldata claim <id> <secret> | refund <id>
//!   calldata lock-id <sender> <receiver> <token> <amount> <hash_lock> <refund_after>
//!   calldata hash-lock <secret>
//!   calldata secret-from-log <contract> <id> <hash_lock> <topic0> <topic1> <data>
use evm_htlc::*;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let addr = |i: usize| parse_address(&a[i]).expect("address");
    let word = |i: usize| parse_word(&a[i]).expect("32-byte word");
    let num = |i: usize| a[i].parse::<u128>().expect("number");
    let out = match a[0].as_str() {
        "lock" => hex(&lock(&addr(1), &word(2), num(3) as u64)),
        "claim" => hex(&claim(&word(1), &word(2))),
        "refund" => hex(&refund(&word(1))),
        "lock-id" => hex(&lock_id(&addr(1), &addr(2), &addr(3), num(4), &word(5), num(6) as u64)),
        "hash-lock" => hex(&hash_lock(&word(1))),
        "secret-from-log" => {
            let topics = [word(4), word(5)];
            let data = parse_hex(&a[6]).expect("data");
            let log = Log { address: &addr(1), topics: &topics, data: &data };
            claimed_secret(&log, &addr(1), &word(2), &word(3)).map_or("none".into(), |s| hex(&s))
        }
        other => panic!("unknown command {other}"),
    };
    println!("{out}");
}
