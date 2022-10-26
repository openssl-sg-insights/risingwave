// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use parking_lot::RwLock;
use risingwave_common::catalog::TableId;
use risingwave_hummock_sdk::HummockEpoch;
use risingwave_pb::hummock::pin_version_response;
use tokio::sync::oneshot;

use crate::hummock::shared_buffer::shared_buffer_batch::SharedBufferBatch;
use crate::hummock::store::memtable::ImmutableMemtable;
use crate::hummock::store::version::HummockReadVersion;
use crate::hummock::HummockResult;
use crate::store::SyncResult;

pub mod hummock_event_handler;
pub use hummock_event_handler::HummockEventHandler;

#[derive(Debug)]
pub struct BufferWriteRequest {
    pub batch: SharedBufferBatch,
    pub epoch: HummockEpoch,
    pub grant_sender: oneshot::Sender<()>,
}

pub enum HummockEvent {
    /// Notify that we may flush the shared buffer.
    BufferMayFlush,

    /// An epoch is going to be synced. Once the event is processed, there will be no more flush
    /// task on this epoch. Previous concurrent flush task join handle will be returned by the join
    /// handle sender.
    SyncEpoch {
        new_sync_epoch: HummockEpoch,
        sync_result_sender: oneshot::Sender<HummockResult<SyncResult>>,
    },

    /// Clear shared buffer and reset all states
    Clear(oneshot::Sender<()>),

    Shutdown,

    VersionUpdate(pin_version_response::Payload),

    ImmToUploader(ImmutableMemtable),

    SealEpoch {
        epoch: HummockEpoch,
        is_checkpoint: bool,
    },

    RegisterHummockInstance {
        table_id: TableId,
        instance_id: u64,
        read_version: Arc<RwLock<HummockReadVersion>>,
        sync_result_sender: oneshot::Sender<()>,
    },

    DestroyHummockInstance {
        table_id: TableId,
        instance_id: u64,
    },
}

impl HummockEvent {
    fn to_debug_string(&self) -> String {
        match self {
            HummockEvent::BufferMayFlush => "BufferMayFlush".to_string(),

            HummockEvent::SyncEpoch {
                new_sync_epoch,
                sync_result_sender: _,
            } => format!("SyncEpoch epoch {} ", new_sync_epoch),

            HummockEvent::Clear(_) => "Clear".to_string(),

            HummockEvent::Shutdown => "Shutdown".to_string(),

            HummockEvent::VersionUpdate(pin_version_response) => {
                format!("VersionUpdate {:?}", pin_version_response)
            }

            HummockEvent::ImmToUploader(imm) => format!("ImmToUploader {:?}", imm),

            HummockEvent::SealEpoch {
                epoch,
                is_checkpoint,
            } => format!(
                "SealEpoch epoch {:?} is_checkpoint {:?}",
                epoch, is_checkpoint
            ),
            HummockEvent::RegisterHummockInstance {
                table_id,
                instance_id,
                read_version: _,
                sync_result_sender: _,
            } => format!(
                "RegisterHummockInstance table_id {:?} instance_id {:?}",
                table_id, instance_id
            ),
            HummockEvent::DestroyHummockInstance {
                table_id,
                instance_id,
            } => format!(
                "DestroyHummockInstance table_id {:?} instance_id {:?}",
                table_id, instance_id
            ),
        }
    }
}

impl std::fmt::Debug for HummockEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HummockEvent")
            .field("debug_string", &self.to_debug_string())
            .finish()
    }
}
