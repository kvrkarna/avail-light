//! Client for Polkadot and Substrate-compatible chains.
//!
//! # Overview of a blockchain
//!
//! A blockchain is, in its essence, a distributed and decentralized key-value database. The
//! principle of a blockchain is to make it possible for any participant to perform modifications
//! to this database, and for all participants to eventually agree on the current state of said
//! database.
//!
//! In Polkadot and Substrate-compatible chains, the state of this database is referred to as
//! "the storage". The storage can be seen more or less as a very large `HashMap`.
//!
//! A blockchain therefore consists in three main things:
//!
//! - The initial state of the storage at the moment when the blockchain starts.
//! - A list of blocks, where each block represents a group of modifications performed to the
//! storage.
//! - A peer-to-peer network of clients connected to each other and exchanging information such
//! as newly-produced blocks.
//!
//! Blocks are built on top of each other, forming a sequential list of modifications to the
//! storage on top of its initial state.
//!
//! ## Blocks
//!
//! A block primarily consists in three properties:
//!
//! - A parent block, referred to by its hash.
//! - An ordered list of **extrinsics**. An extrinsic can be either a **transaction** or an
//! **intrisic**.
//! - A list of digest items, which include for example a cryptographic signature of the block
//! made by its author.
//!
//! In order to make abstractions easier, there alsos exists what is called the genesis block, or
//! block number 0. It doesn't have any parent, extrinsic, or digest item. The state of the
//! storage of the genesis block is the initial state.
//!
//! From these three block properties, we can derive:
//!
//! - The hash of the block. This is a unique 256 bits identifier obtained by hashing all the
//! information together in a specific way.
//! - The block number. It is equal to the parent's block number plus one, or equal to zero for
//! the genesis block
//! - The state of the storage at the height of the block.The state at the height of the block
//! consists in the state of the parent block on top of which we have applied the block's
//! extrinsics on top of each other.
//!
//! ## Trie
//!
//! The **trie** is a data structure that plays an important part in the way a blockchain
//! functions. It consists in a tree of keys and values whose content can be hashed. This hash is
//! commonly designated as "the Merkle trie root" or "the trie root".
//! See the [`trie`] module for more details.
//!
//! ## Block headers
//!
//! A block's header contains the following information:
//!
//! - The hash of the parent block.
//! - The block number.
//! - The state trie root, which consists in the trie root of all the keys and values of the
//! storage after this block's modifications have been applied.
//! - The extrinsics trie root, which consists in the Merkle root of a trie containing the
//! extrinsics of the block.
//! - The list of digest items.
//!
//! ## Finalization
//!
//! Each block of a chain can be or not **finalized** in the context of a given chain. Once a
//! block has been finalized, we consider as invalid any block that is not a descendant of it. In
//! other words, a finalized block can never be reverted and is forever part of the chain.
//!
//! By extension, the parent of a finalized block must be finalized as well. The genesis block of
//! a chain is by definition always finalized.
//!
//! # TODO: what's a justification?
//!

// TODO: fix all the compilation errors caused by the copy-pasted code
//#![cfg_attr(not(test), no_std)]
#![recursion_limit = "512"]
#![feature(new_uninit)] // TODO:
#![feature(asm)] // TODO:

extern crate alloc;

pub mod babe;
pub mod block; // TODO: remove
pub mod block_import;
pub mod chain_spec;
pub mod database;
pub mod executor;
pub mod header;
pub mod informant;
pub mod keystore;
pub mod network;
pub mod rpc_server;
pub mod service;
pub mod trie;

use core::iter;
use parity_scale_codec::Encode as _;

/// Calculates the hash of the genesis block from the storage.
///
/// # Context
///
/// A blockchain is a key-value database. Each block built at the head of the chain updates
/// entries in this key-value database.
///
/// In order to make things easier, there exists a special block whose number is 0 and that
/// is called the genesis block. Block number 1 while have as parent the genesis block (then,
/// block number 2 has block number 1 as parent, and so on).
///
/// The hash of the genesis block depends purely on the initial state of the content.
/// This function makes it possible to calculate this hash.
pub fn calculate_genesis_block_hash<'a>(
    genesis_storage: impl Iterator<Item = (&'a [u8], &'a [u8])> + Clone,
) -> [u8; 32] {
    let state_root = {
        let mut calculation = trie::calculate_root::root_merkle_value(None);

        loop {
            match calculation {
                trie::calculate_root::RootMerkleValueCalculation::Finished { hash, .. } => {
                    break hash
                }
                trie::calculate_root::RootMerkleValueCalculation::AllKeys(keys) => {
                    calculation =
                        keys.inject(genesis_storage.clone().map(|(k, _)| k.iter().cloned()));
                }
                trie::calculate_root::RootMerkleValueCalculation::StorageValue(val) => {
                    // TODO: don't allocate
                    let key = val.key().collect::<Vec<_>>();
                    let value = genesis_storage
                        .clone()
                        .find(|(k, v)| k == &key)
                        .map(|(k, v)| v);
                    calculation = val.inject(value);
                }
            }
        }
    };

    let genesis_block_header = header::HeaderRef {
        parent_hash: &[0; 32],
        number: 0,
        state_root: &state_root,
        extrinsics_root: &trie::empty_trie_merkle_value(),
        digest: header::DigestRef::empty(),
    };

    genesis_block_header.hash()
}

/// Turns a [`database::DatabaseOpen`] into a [`database::Database`], either by inserting the
/// genesis block into a newly-created database, or by checking when the existing database matches
/// the chain specs.
pub fn database_open_match_chain_specs(
    database: database::DatabaseOpen,
    chain_spec: &chain_spec::ChainSpec,
) -> Result<database::Database, database::AccessError> {
    match database {
        database::DatabaseOpen::Open(database) => {
            // TODO: verify that the database matches the chain spec
            Ok(database)
        }
        database::DatabaseOpen::Empty(empty) => {
            // TODO: quite a bit of code duplication here
            let mut state_trie = trie::Trie::new();
            for (key, value) in chain_spec.genesis_storage() {
                state_trie.insert(key, value.to_vec());
            }

            let genesis_block_header = block::Header {
                parent_hash: [0; 32].into(),
                number: 0,
                state_root: state_trie.root_merkle_value(None).into(),
                extrinsics_root: trie::Trie::new().root_merkle_value(None).into(),
                digest: block::Digest { logs: Vec::new() },
            }
            .encode();

            empty.insert_genesis_block(&genesis_block_header, chain_spec.genesis_storage())
        }
    }
}
