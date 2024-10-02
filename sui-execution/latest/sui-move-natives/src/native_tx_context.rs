// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use better_any::{Tid, TidAble};
use move_core_types::account_address::AccountAddress;
use sui_types::base_types::{EpochId, TransactionDigest, TxContext};
use sui_types::messages_checkpoint::CheckpointTimestamp;

#[derive(Debug, Tid)]
pub struct NativeTxContext {
    /// Signer/sender of the transaction
    pub sender: AccountAddress,
    /// Digest of the current transaction
    pub digest: TransactionDigest,
    /// The current epoch number
    pub epoch: EpochId,
    /// Timestamp that the epoch started at
    pub epoch_timestamp_ms: CheckpointTimestamp,
    /// Number of `ObjectID`'s generated during execution of the current transaction
    pub ids_created: u64,
}

impl From<&TxContext> for NativeTxContext {
    fn from(tx_context: &TxContext) -> Self {
        NativeTxContext {
            sender: AccountAddress::new(tx_context.sender().to_inner()),
            digest: tx_context.digest(),
            epoch: tx_context.epoch(),
            epoch_timestamp_ms: tx_context.epoch_timestamp_ms(),
            ids_created: 0,
        }
    }
}
