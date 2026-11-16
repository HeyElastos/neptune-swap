//! Scanner tests on fake blocks: invalid proofs and PoW, real kernels,
//! announcements and mutator-set bookkeeping.

use neptune_consensus::block::block_transaction::BlockTransaction;
use neptune_consensus::block::Block;
use neptune_consensus::transaction::announcement::Announcement;
use neptune_consensus::transaction::transaction_kernel::TransactionKernelModifier;
use neptune_consensus::transaction::transaction_kernel::TransactionKernelProxy;
use neptune_consensus::transaction::Transaction;
use neptune_consensus::transaction::TransactionProof;
use neptune_mutator_set::addition_record::AdditionRecord;
use neptune_mutator_set::commit;
use neptune_mutator_set::removal_record::chunk_dictionary::ChunkDictionary;
use neptune_mutator_set::removal_record::RemovalRecord;
use neptune_wallet::address::generation_address::GenerationSpendingKey;
use neptune_wallet::coinbase_distribution::CoinbaseDistribution;
use neptune_wallet::composer_parameters::prepare_coinbase_transaction_stateless;
use neptune_wallet::composer_parameters::ComposerParameters;
use neptune_wallet::fee_notification_policy::FeeNotificationPolicy;
use neptune_wallet::utxo_notification::UtxoNotificationPayload;
use rand::RngExt;
use tasm_lib::twenty_first::util_types::mmr::mmr_trait::Mmr;

use super::*;

const NETWORK: Network = Network::Main;
const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon \
                      abandon abandon abandon abandon abandon abandon abandon abandon agent";

/// A block after `prev` carrying the given extra outputs, inputs and
/// announcements, plus a coinbase to a random stranger (not announced).
fn block_after(
    prev: &Block,
    outputs: Vec<AdditionRecord>,
    inputs: Vec<RemovalRecord>,
    announcements: Vec<Announcement>,
) -> Block {
    let mut rng = rand::rng();
    let stranger = GenerationSpendingKey::derive_from_seed(rng.random());
    let timestamp = prev.header().timestamp + NETWORK.target_block_interval();
    let params = ComposerParameters::new(
        CoinbaseDistribution::solo(stranger.to_address().into()),
        rng.random(),
        Some(stranger.receiver_preimage()),
        0.5,
        FeeNotificationPolicy::OffChain,
    );
    let (_, details) = prepare_coinbase_transaction_stateless(prev, params, timestamp, NETWORK);
    let kernel = details.primitive_witness().kernel;
    let proxy = TransactionKernelProxy::from(kernel.clone());
    let kernel = TransactionKernelModifier::default()
        .outputs([proxy.outputs, outputs].concat())
        .inputs([proxy.inputs, inputs].concat())
        .announcements([proxy.announcements, announcements].concat())
        .modify(kernel);
    let tx = BlockTransaction::upgrade(Transaction { kernel, proof: TransactionProof::invalid() });
    let mut block = Block::block_template_invalid_proof(prev, tx, timestamp, None, NETWORK);
    block.set_header_guesser_data(stranger.to_address().into());
    block
}

/// A payment of `coins` NPT to `key`: its addition record and announcement.
fn payment(key: &SpendingKey, coins: u32) -> (AdditionRecord, Announcement) {
    let address = key.to_address();
    let utxo = Utxo::new_native_currency(address.lock_script_hash(), NativeCurrencyAmount::coins(coins));
    let sender_randomness: Digest = rand::rng().random();
    let record = commit(Tip5::hash(&utxo), sender_randomness, address.privacy_digest());
    let announcement = address.generate_announcement(UtxoNotificationPayload::new(utxo, sender_randomness));
    (record, announcement)
}

fn wallet_at(block: &Block) -> LightWallet {
    let mut w = LightWallet::from_phrase(PHRASE, NETWORK).unwrap();
    w.set_sync_point(SyncPoint { height: block.header().height.into(), hash: block.hash() });
    w
}

fn rpc(block: &Block) -> RpcWalletBlock {
    RpcWalletBlock::from(block)
}

#[test]
fn receives_and_spends() {
    let genesis = Block::genesis(NETWORK);
    let mut w = wallet_at(&genesis);
    let entropy = WalletEntropy::from_phrase(PHRASE.split_whitespace()).unwrap();
    let gen0: SpendingKey = entropy.nth_generation_spending_key(0).into();
    let change0: SpendingKey = entropy.nth_symmetric_key(0).into();

    // Block 1: 7 NPT to the default address, 3 NPT to a stranger.
    let (mine, note) = payment(&gen0, 7);
    let stranger: SpendingKey = GenerationSpendingKey::derive_from_seed(rand::rng().random()).into();
    let (theirs, their_note) = payment(&stranger, 3);
    let b1 = block_after(&genesis, vec![theirs, mine], vec![], vec![their_note, note]);
    let r = w.apply_blocks(vec![rpc(&b1)]).unwrap();
    assert_eq!((r.received, r.spent), (1, 0));
    assert_eq!(w.balance(u64::MAX).total, "7");

    // The coin's AOCL index is where the mutator set put it.
    let coin = w.state().coins[0].clone();
    let msa_after = b1.mutator_set_accumulator_after().unwrap();
    let prior = b1.body().num_aocl_leafs_prior();
    assert!(coin.aocl_index >= prior && coin.aocl_index < msa_after.aocl.num_leafs());

    // Block 2: spend it, 5 NPT change back to the wallet's symmetric key.
    let spend = RemovalRecord { absolute_indices: index_set_of(&coin), target_chunks: ChunkDictionary::default() };
    let (change, change_note) = payment(&change0, 5);
    let b2 = block_after(&b1, vec![change], vec![spend], vec![change_note]);
    let r = w.apply_blocks(vec![rpc(&b2)]).unwrap();
    assert_eq!((r.received, r.spent), (1, 1));
    let b = w.balance(u64::MAX);
    assert_eq!(b.total, "5");
    assert_eq!((b.unspent_coins, b.spent_coins), (1, 1));
    assert_eq!(w.state().sync.unwrap().height, 2);
}

#[test]
fn announcement_without_output_is_ignored() {
    let genesis = Block::genesis(NETWORK);
    let mut w = wallet_at(&genesis);
    let entropy = WalletEntropy::from_phrase(PHRASE.split_whitespace()).unwrap();
    let (_, note) = payment(&entropy.nth_generation_spending_key(0).into(), 1000);
    let b1 = block_after(&genesis, vec![], vec![], vec![note]);
    let r = w.apply_blocks(vec![rpc(&b1)]).unwrap();
    assert_eq!(r.received, 0);
    assert_eq!(w.balance(u64::MAX).unspent_coins, 0);
}

#[test]
fn key_window_grows_past_the_gap() {
    let genesis = Block::genesis(NETWORK);
    let mut w = wallet_at(&genesis);
    let entropy = WalletEntropy::from_phrase(PHRASE.split_whitespace()).unwrap();
    // Index 20 is inside the first window; 40 only after the wallet derives
    // more keys because 20 received. Both arrive in the same block.
    let (r20, n20) = payment(&entropy.nth_generation_spending_key(20).into(), 1);
    let (r40, n40) = payment(&entropy.nth_generation_spending_key(40).into(), 2);
    let b1 = block_after(&genesis, vec![r20, r40], vec![], vec![n20, n40]);
    let r = w.apply_blocks(vec![rpc(&b1)]).unwrap();
    assert_eq!(r.received, 2);
    assert_eq!(w.state().derived[0], 41 + KEY_GAP);
}

#[test]
fn rejects_a_block_that_does_not_connect() {
    let genesis = Block::genesis(NETWORK);
    let b1 = block_after(&genesis, vec![], vec![], vec![]);
    let other_b1 = block_after(&genesis, vec![], vec![], vec![]);
    let b2 = block_after(&other_b1, vec![], vec![], vec![]);
    let mut w = wallet_at(&genesis);
    w.apply_blocks(vec![rpc(&b1)]).unwrap();
    let err = w.apply_blocks(vec![rpc(&b2)]).unwrap_err().to_string();
    assert!(err.contains("does not follow"), "{err}");
}

#[test]
fn state_round_trips_through_json_and_resume() {
    let genesis = Block::genesis(NETWORK);
    let mut w = wallet_at(&genesis);
    let entropy = WalletEntropy::from_phrase(PHRASE.split_whitespace()).unwrap();
    let (rec, note) = payment(&entropy.nth_ec_hybrid_key(3).into(), 4);
    let b1 = block_after(&genesis, vec![rec], vec![], vec![note]);
    w.apply_blocks(vec![rpc(&b1)]).unwrap();

    let json = serde_json::to_string(w.state()).unwrap();
    let state: WalletState = serde_json::from_str(&json).unwrap();
    let w2 = LightWallet::resume(PHRASE, state).unwrap();
    assert_eq!(w2.balance(u64::MAX).total, "4");
    assert_eq!(w2.state().sync, w.state().sync);
    assert_eq!(w2.unspent_index_sets().len(), 1);
}

#[test]
fn rpc_bodies_match_the_node_format() {
    let body: Value = serde_json::from_str(&rpc::get_blocks_request(5, 9)).unwrap();
    assert_eq!(body["method"], "wallet_getBlocks");
    assert_eq!(body["params"], serde_json::json!([5, 9]));
    assert_eq!(rpc::parse_height(r#"{"jsonrpc":"2.0","id":1,"result":{"height":60134}}"#).unwrap(), 60134);
    assert!(rpc::parse_height(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1}}"#).is_err());
}

use serde_json::Value;

/// Real `wallet_getBlocks` replies, saved to files named in `NEPTUNE_BLOCKS`
/// (comma-separated). Each run of blocks must chain: the wallet's hash of every
/// block is the next block's `prev_block_digest`. Across main net height 62,500
/// this covers softfork epsilon's larger block appendix.
#[test]
#[ignore = "needs saved node replies: NEPTUNE_BLOCKS=a.json,b.json"]
fn real_blocks_chain_through_the_wallet() {
    let files = std::env::var("NEPTUNE_BLOCKS").expect("NEPTUNE_BLOCKS");
    for file in files.split(',') {
        let reply = std::fs::read_to_string(file).unwrap();
        let blocks = rpc::parse_blocks(&reply).unwrap();
        let first: BlockKernel = blocks[0].kernel.clone().into();
        let mut w = LightWallet::from_phrase(PHRASE, NETWORK).unwrap();
        let start = u64::from(first.header.height) - 1;
        w.set_sync_point(SyncPoint { height: start, hash: first.header.prev_block_digest });
        let n = blocks.len() as u64;
        let r = w.apply_blocks(blocks).unwrap();
        assert_eq!(r.height, Some(start + n), "{file}");
        eprintln!("{file}: blocks {}..={} chain; {} inputs, {} announcements", start + 1, start + n, r.inputs, r.announcements);
    }
}

#[test]
fn amounts_drop_trailing_zeros() {
    assert_eq!(npt(NativeCurrencyAmount::coins(25)), "25");
    assert_eq!(npt(NativeCurrencyAmount::coins_from_str("14.95").unwrap()), "14.95");
    assert_eq!(npt(NativeCurrencyAmount::coins(0)), "0");
}
