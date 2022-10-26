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

use std::collections::HashMap;
use std::iter::once;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use futures::future::{select, try_join_all, Either};
use futures::FutureExt;
use itertools::Itertools;
use parking_lot::RwLock;
use risingwave_common::catalog::TableId;
use risingwave_common::config::StorageConfig;
use risingwave_hummock_sdk::compaction_group::hummock_version_ext::HummockVersionExt;
use risingwave_hummock_sdk::HummockEpoch;
use risingwave_pb::hummock::pin_version_response::Payload;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use crate::hummock::compactor::Context;
use crate::hummock::conflict_detector::ConflictDetector;
use crate::hummock::event_handler::HummockEvent;
use crate::hummock::local_version::local_version_manager::LocalVersionManager;
use crate::hummock::local_version::pinned_version::PinnedVersion;
use crate::hummock::local_version::upload_handle_manager::UploadHandleManager;
use crate::hummock::local_version::SyncUncommittedDataStage;
use crate::hummock::store::memtable::ImmutableMemtable;
use crate::hummock::store::state_store::HummockStorage;
use crate::hummock::store::version::{HummockReadVersion, VersionUpdate};
use crate::hummock::utils::validate_table_key_range;
use crate::hummock::{HummockError, HummockResult, MemoryLimiter, TrackerId};
use crate::store::SyncResult;

#[derive(Clone)]
pub struct BufferTracker {
    flush_threshold: usize,
    global_buffer: Arc<MemoryLimiter>,
    global_upload_task_size: Arc<AtomicUsize>,
}

impl BufferTracker {
    pub fn from_storage_config(config: &StorageConfig) -> Self {
        let capacity = config.shared_buffer_capacity_mb as usize * (1 << 20);
        let flush_threshold = capacity * 4 / 5;
        Self {
            flush_threshold,
            global_buffer: Arc::new(MemoryLimiter::new(capacity as u64)),
            global_upload_task_size: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn get_buffer_size(&self) -> usize {
        self.global_buffer.get_memory_usage() as usize
    }

    pub fn get_memory_limiter(&self) -> &Arc<MemoryLimiter> {
        &self.global_buffer
    }

    pub fn global_upload_task_size(&self) -> Arc<AtomicUsize> {
        self.global_upload_task_size.clone()
    }

    /// Return true when the buffer size minus current upload task size is still greater than the
    /// flush threshold.
    pub fn need_more_flush(&self) -> bool {
        self.get_buffer_size()
            > self.flush_threshold + self.global_upload_task_size.load(Ordering::Relaxed)
    }
}

type InstanceId = u64;
pub type ReadVersionMappingType =
    RwLock<HashMap<TableId, HashMap<InstanceId, Arc<RwLock<HummockReadVersion>>>>>;

pub struct HummockEventHandler {
    buffer_tracker: BufferTracker,
    // sstable_id_manager: SstableIdManagerRef,
    hummock_event_rx: mpsc::UnboundedReceiver<HummockEvent>,
    upload_handle_manager: UploadHandleManager,
    pending_sync_requests: HashMap<HummockEpoch, oneshot::Sender<HummockResult<SyncResult>>>,
    read_version_mapping: Arc<ReadVersionMappingType>,
    version_update_notifier_tx: Arc<tokio::sync::watch::Sender<HummockEpoch>>,
    seal_epoch: Arc<AtomicU64>,
    pinned_version: PinnedVersion,
    write_conflict_detector: Option<Arc<ConflictDetector>>,
    local_version_manager: Arc<LocalVersionManager>,
    context: Arc<Context>,
}

impl HummockEventHandler {
    pub fn new(
        local_version_manager: Arc<LocalVersionManager>,
        hummock_event_rx: mpsc::UnboundedReceiver<HummockEvent>,
        pinned_version: PinnedVersion,
        compactor_context: Arc<Context>,
    ) -> Self {
        let seal_epoch = Arc::new(AtomicU64::new(pinned_version.max_committed_epoch()));
        let (version_update_notifier_tx, _) =
            tokio::sync::watch::channel(pinned_version.max_committed_epoch());
        let version_update_notifier_tx = Arc::new(version_update_notifier_tx);
        let write_conflict_detector = ConflictDetector::new_from_config(&compactor_context.options);
        let read_version_mapping = Arc::new(RwLock::new(HashMap::default()));
        Self {
            buffer_tracker: local_version_manager.buffer_tracker().clone(),
            hummock_event_rx,
            upload_handle_manager: UploadHandleManager::new(),
            pending_sync_requests: Default::default(),
            version_update_notifier_tx,
            seal_epoch,
            pinned_version,
            write_conflict_detector,
            local_version_manager,
            read_version_mapping,
            context: compactor_context,
        }
    }

    pub fn sealed_epoch(&self) -> Arc<AtomicU64> {
        self.seal_epoch.clone()
    }

    pub fn version_update_notifier_tx(&self) -> Arc<tokio::sync::watch::Sender<HummockEpoch>> {
        self.version_update_notifier_tx.clone()
    }

    pub fn read_version_mapping(&self) -> Arc<ReadVersionMappingType> {
        self.read_version_mapping.clone()
    }

    pub fn buffer_tracker(&self) -> &BufferTracker {
        &self.buffer_tracker
    }

    pub fn pinned_version(&self) -> PinnedVersion {
        self.pinned_version.clone()
    }

    fn try_flush_shared_buffer(&mut self) {
        // Keep issuing new flush task until flush is not needed or we can issue
        // no more task
        while self.buffer_tracker.need_more_flush() {
            if let Some((epoch, join_handle)) =
                self.local_version_manager.clone().flush_shared_buffer()
            {
                self.upload_handle_manager
                    .add_epoch_handle(epoch, once(join_handle));
            } else {
                break;
            }
        }
    }

    fn send_sync_result(&mut self, epoch: HummockEpoch, result: HummockResult<SyncResult>) {
        if let Some(tx) = self.pending_sync_requests.remove(&epoch) {
            let _ = tx.send(result).inspect_err(|e| {
                error!("unable to send sync result. Epoch: {}. Err: {:?}", epoch, e);
            });
        } else {
            panic!("send sync result to non-requested epoch: {}", epoch);
        }
    }
}

// Handler for different events
impl HummockEventHandler {
    fn handle_epoch_finished(&mut self, epoch: HummockEpoch) {
        // TODO: in some case we may only need the read guard.
        let mut local_version_guard = self.local_version_manager.local_version.write();
        if epoch > local_version_guard.get_max_sync_epoch() {
            // The finished flush task does not belong to any syncing epoch.
            return;
        }
        let sync_epoch = epoch;
        let compaction_group_index = local_version_guard
            .pinned_version()
            .compaction_group_index();
        let sync_data = local_version_guard
            .sync_uncommitted_data
            .get_mut(&sync_epoch)
            .expect("should find");
        match sync_data.stage() {
            SyncUncommittedDataStage::CheckpointEpochSealed(_) => {
                let (payload, sync_size) = sync_data.start_syncing();
                let local_version_manager = self.local_version_manager.clone();
                let join_handle = tokio::spawn(async move {
                    let _ = local_version_manager
                        .run_sync_upload_task(
                            payload,
                            compaction_group_index,
                            sync_size,
                            sync_epoch,
                        )
                        .await
                        .inspect_err(|e| {
                            error!("sync upload task failed: {}, err: {:?}", sync_epoch, e);
                        });
                });
                self.upload_handle_manager
                    .add_epoch_handle(sync_epoch, once(join_handle));
            }
            SyncUncommittedDataStage::Syncing(_) => {
                unreachable!("when a join handle is finished, the stage should not be at syncing");
            }
            SyncUncommittedDataStage::Failed(_) => {
                drop(local_version_guard);
                self.send_sync_result(sync_epoch, Err(HummockError::other("sync task failed")));
            }
            SyncUncommittedDataStage::Synced(ssts, sync_size) => {
                let ssts = ssts.clone();
                let sync_size = *sync_size;
                drop(local_version_guard);
                self.send_sync_result(
                    sync_epoch,
                    Ok(SyncResult {
                        sync_size,
                        uncommitted_ssts: ssts,
                    }),
                );
            }
        }
    }

    fn handle_sync_epoch(
        &mut self,
        new_sync_epoch: HummockEpoch,
        sync_result_sender: oneshot::Sender<HummockResult<SyncResult>>,
    ) {
        if let Some(old_sync_result_sender) = self
            .pending_sync_requests
            .insert(new_sync_epoch, sync_result_sender)
        {
            let _ = old_sync_result_sender
                .send(Err(HummockError::other(
                    "the sync rx is overwritten by an new rx",
                )))
                .inspect_err(|e| {
                    error!(
                        "unable to send sync result: {}. Err: {:?}",
                        new_sync_epoch, e
                    );
                });
        }
        let mut local_version_guard = self.local_version_manager.local_version.write();
        let prev_max_sync_epoch =
            if let Some(epoch) = local_version_guard.get_prev_max_sync_epoch(new_sync_epoch) {
                epoch
            } else {
                drop(local_version_guard);
                self.send_sync_result(
                    new_sync_epoch,
                    Err(HummockError::other(format!(
                        "no sync task on epoch: {}. May have been cleared",
                        new_sync_epoch
                    ))),
                );
                return;
            };
        let flush_join_handles = self
            .upload_handle_manager
            .drain_epoch_handle(prev_max_sync_epoch + 1..=new_sync_epoch);
        if flush_join_handles.is_empty() {
            // no pending flush to wait. Start syncing

            let (payload, sync_size) = local_version_guard.start_syncing(new_sync_epoch);
            let compaction_group_index = local_version_guard
                .pinned_version()
                .compaction_group_index();
            let local_version_manager = self.local_version_manager.clone();
            let join_handle = tokio::spawn(async move {
                let _ = local_version_manager
                    .run_sync_upload_task(
                        payload,
                        compaction_group_index,
                        sync_size,
                        new_sync_epoch,
                    )
                    .await
                    .inspect_err(|e| {
                        error!("sync upload task failed: {}, err: {:?}", new_sync_epoch, e);
                    });
            });
            self.upload_handle_manager
                .add_epoch_handle(new_sync_epoch, once(join_handle));
        } else {
            // some pending flush task. waiting for flush to finish.
            // Note: the flush join handle of some previous epoch is now attached to
            // the new sync epoch
            self.upload_handle_manager
                .add_epoch_handle(new_sync_epoch, flush_join_handles.into_iter());
        }
    }

    async fn handle_clear(&mut self, notifier: oneshot::Sender<()>) {
        // Wait for all ongoing flush to finish.
        let ongoing_flush_handles: Vec<_> = self.upload_handle_manager.drain_epoch_handle(..);
        if let Err(e) = try_join_all(ongoing_flush_handles).await {
            error!("Failed to join flush handle {:?}", e)
        }

        // There cannot be any pending write requests since we should only clear
        // shared buffer after all actors stop processing data.
        let pending_epochs = self.pending_sync_requests.keys().cloned().collect_vec();
        pending_epochs.into_iter().for_each(|epoch| {
            self.send_sync_result(
                epoch,
                Err(HummockError::other("the pending sync is cleared")),
            );
        });

        // Clear shared buffer
        self.local_version_manager
            .local_version
            .write()
            .clear_shared_buffer();
        self.context
            .sstable_id_manager
            .remove_watermark_sst_id(TrackerId::Epoch(HummockEpoch::MAX));

        // Notify completion of the Clear event.
        notifier.send(()).unwrap();
    }

    fn handle_version_update(&mut self, version_payload: Payload) {
        let prev_max_committed_epoch = self.pinned_version.max_committed_epoch();
        // TODO: after local version manager is removed, we can match version_payload directly
        // instead of taking a reference
        let newly_pinned_version = match &version_payload {
            Payload::VersionDeltas(version_deltas) => {
                let mut version_to_apply = self.pinned_version.version();
                for version_delta in &version_deltas.version_deltas {
                    assert_eq!(version_to_apply.id, version_delta.prev_id);
                    version_to_apply.apply_version_delta(version_delta);
                }
                version_to_apply
            }
            Payload::PinnedVersion(version) => version.clone(),
        };

        validate_table_key_range(&newly_pinned_version);

        self.pinned_version = self.pinned_version.new_pin_version(newly_pinned_version);

        {
            let read_version_mapping_guard = self.read_version_mapping.read();

            // todo: do some prune for version update
            read_version_mapping_guard
                .values()
                .flat_map(HashMap::values)
                .for_each(|read_version| {
                    read_version
                        .write()
                        .update(VersionUpdate::CommittedSnapshot(
                            self.pinned_version.clone(),
                        ))
                });
        }

        let max_committed_epoch = self.pinned_version.max_committed_epoch();

        // only notify local_version_manager when MCE change
        self.version_update_notifier_tx.send_if_modified(|state| {
            assert_eq!(prev_max_committed_epoch, *state);
            if max_committed_epoch > *state {
                *state = max_committed_epoch;
                true
            } else {
                false
            }
        });

        if let Some(conflict_detector) = self.write_conflict_detector.as_ref() {
            conflict_detector.set_watermark(self.pinned_version.max_committed_epoch());
        }
        self.context
            .sstable_id_manager
            .remove_watermark_sst_id(TrackerId::Epoch(self.pinned_version.max_committed_epoch()));

        // this is only for clear the committed data in local version
        // TODO: remove it
        self.local_version_manager
            .try_update_pinned_version(version_payload);
    }

    fn handle_imm_to_uploader(&self, imm: ImmutableMemtable) {
        self.local_version_manager.write_shared_buffer_batch(imm);
    }
}

impl HummockEventHandler {
    pub async fn start_hummock_event_handler_worker(mut self) {
        loop {
            let select_result = match select(
                self.upload_handle_manager.next_finished_epoch(),
                self.hummock_event_rx.recv().boxed(),
            )
            .await
            {
                Either::Left((epoch_result, _)) => Either::Left(epoch_result),
                Either::Right((event, _)) => Either::Right(event),
            };
            match select_result {
                Either::Left(epoch_result) => {
                    let epoch = epoch_result.expect(
                        "now we don't cancel the join handle. So join is expected to be success",
                    );
                    self.handle_epoch_finished(epoch);
                }
                Either::Right(Some(event)) => match event {
                    HummockEvent::BufferMayFlush => {
                        // Only check and flush shared buffer after batch has been added to shared
                        // buffer.
                        self.try_flush_shared_buffer();
                    }

                    HummockEvent::SyncEpoch {
                        new_sync_epoch,
                        sync_result_sender,
                    } => {
                        self.handle_sync_epoch(new_sync_epoch, sync_result_sender);
                    }
                    HummockEvent::Clear(notifier) => {
                        self.handle_clear(notifier).await;
                    }
                    HummockEvent::Shutdown => {
                        info!("buffer tracker shutdown");
                        break;
                    }

                    HummockEvent::VersionUpdate(version_payload) => {
                        self.handle_version_update(version_payload);
                    }

                    HummockEvent::ImmToUploader(imm) => {
                        self.handle_imm_to_uploader(imm);
                    }

                    HummockEvent::SealEpoch {
                        epoch,
                        is_checkpoint,
                    } => {
                        self.local_version_manager
                            .local_version
                            .write()
                            .seal_epoch(epoch, is_checkpoint);

                        self.seal_epoch.store(epoch, Ordering::SeqCst);
                    }

                    HummockEvent::RegisterHummockInstance {
                        table_id,
                        instance_id,
                        event_tx_for_instance,
                        sync_result_sender,
                    } => {
                        let basic_read_version = Arc::new(RwLock::new(HummockReadVersion::new(
                            self.pinned_version.clone(),
                        )));

                        let storage_instance = HummockStorage::new(
                            self.context.options.clone(),
                            self.context.sstable_store.clone(),
                            self.context.hummock_meta_client.clone(),
                            self.context.stats.clone(),
                            basic_read_version.clone(),
                            event_tx_for_instance.clone(),
                            self.buffer_tracker().get_memory_limiter().clone(),
                        )
                        .expect("storage_core mut be init");

                        let mut read_version_mapping_guard = self.read_version_mapping.write();

                        read_version_mapping_guard
                            .entry(table_id)
                            .or_default()
                            .insert(instance_id, basic_read_version);

                        sync_result_sender
                            .send(storage_instance)
                            .expect("RegisterHummockInstance send fail");
                    }

                    HummockEvent::DestroyHummockInstance {
                        table_id,
                        instance_id,
                    } => {
                        let mut read_version_mapping_guard = self.read_version_mapping.write();
                        read_version_mapping_guard
                            .get_mut(&table_id)
                            .unwrap_or_else(|| {
                                panic!(
                                    "DestroyHummockInstance table_id {} instance_id {} fail",
                                    table_id, instance_id
                                )
                            })
                            .remove(&instance_id);
                    }
                },
                Either::Right(None) => {
                    break;
                }
            };
        }
    }
}
