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
use std::ops::Bound::{Included, Unbounded};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use risingwave_common::config::StorageConfig;
use risingwave_hummock_sdk::filter_key_extractor::FilterKeyExtractorManager;
use risingwave_hummock_sdk::HummockEpoch;
use risingwave_meta::hummock::test_utils::setup_compute_env;
use risingwave_meta::hummock::{HummockManagerRef, MockHummockMetaClient};
use risingwave_meta::manager::MetaSrvEnv;
use risingwave_meta::storage::MemStore;
use risingwave_pb::common::WorkerNode;
use risingwave_rpc_client::HummockMetaClient;
use risingwave_storage::hummock::compactor::Context;
use risingwave_storage::hummock::event_handler::hummock_event_handler::BufferTracker;
use risingwave_storage::hummock::event_handler::{HummockEvent, HummockEventHandler};
use risingwave_storage::hummock::iterator::test_utils::mock_sstable_store;
use risingwave_storage::hummock::local_version::local_version_manager::LocalVersionManager;
use risingwave_storage::hummock::store::state_store::HummockStorage;
use risingwave_storage::hummock::store::{ReadOptions, StateStore};
use risingwave_storage::hummock::test_utils::default_config_for_test;
use risingwave_storage::hummock::{SstableIdManager, SstableStore};
use risingwave_storage::monitor::StateStoreMetrics;
use risingwave_storage::storage_value::StorageValue;
use risingwave_storage::store::{SyncResult, WriteOptions};
use risingwave_storage::StateStoreIter;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::test_utils::{prefixed_key, prepare_first_valid_version};

pub async fn prepare_hummock_event_handler(
    opt: Arc<StorageConfig>,
    env: MetaSrvEnv<MemStore>,
    hummock_manager_ref: HummockManagerRef<MemStore>,
    worker_node: WorkerNode,
    sstable_store_ref: Arc<SstableStore>,
) -> (HummockEventHandler, UnboundedSender<HummockEvent>) {
    let (pinned_version, event_tx, event_rx) =
        prepare_first_valid_version(env, hummock_manager_ref.clone(), worker_node.clone()).await;

    let hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));

    let sstable_id_manager = Arc::new(SstableIdManager::new(
        hummock_meta_client.clone(),
        opt.sstable_id_remote_fetch_number,
    ));
    let compactor_context = Arc::new(Context::new_local_compact_context(
        opt.clone(),
        sstable_store_ref,
        hummock_meta_client,
        Arc::new(StateStoreMetrics::unused()),
        sstable_id_manager,
        Arc::new(FilterKeyExtractorManager::default()),
    ));

    let buffer_tracker = BufferTracker::from_storage_config(&opt);

    let read_version_mapping = Arc::new(RwLock::new(HashMap::default()));
    let local_version_manager = LocalVersionManager::new(
        pinned_version.clone(),
        compactor_context.clone(),
        buffer_tracker,
        eevent_tx.clone(),
    );

    let hummock_event_handler = HummockEventHandler::new(
        local_version_manager,
        event_rx,
        pinned_version,
        compactor_context,
        read_version_mapping,
    );

    (hummock_event_handler, event_tx)
}

async fn try_wait_epoch_for_test(
    wait_epoch: u64,
    version_update_notifier_tx: Arc<tokio::sync::watch::Sender<HummockEpoch>>,
) {
    let mut receiver = version_update_notifier_tx.subscribe();
    let max_committed_epoch = *receiver.borrow_and_update();
    if max_committed_epoch >= wait_epoch {
        return;
    }

    match tokio::time::timeout(Duration::from_secs(1), receiver.changed()).await {
        Err(elapsed) => {
            panic!(
                "wait_epoch {:?} timeout when waiting for version update elapsed {:?}s",
                wait_epoch, elapsed
            );
        }
        Ok(Err(_)) => {
            panic!("tx dropped");
        }
        Ok(Ok(_)) => {
            let max_committed_epoch = *receiver.borrow();
            if max_committed_epoch < wait_epoch {
                panic!("max_committed_epoch {:?} update fail", max_committed_epoch);
            }
        }
    }
}

async fn sync_epoch(event_tx: &UnboundedSender<HummockEvent>, epoch: HummockEpoch) -> SyncResult {
    event_tx
        .send(HummockEvent::SealEpoch {
            epoch,
            is_checkpoint: true,
        })
        .unwrap();
    let (tx, rx) = oneshot::channel();
    event_tx
        .send(HummockEvent::SyncEpoch {
            new_sync_epoch: epoch,
            sync_result_sender: tx,
        })
        .unwrap();
    rx.await.unwrap().unwrap()
}

#[tokio::test]
async fn test_storage_basic() {
    let sstable_store = mock_sstable_store();
    let hummock_options = Arc::new(default_config_for_test());
    let (env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));

    let (hummock_event_handler, event_tx) = prepare_hummock_event_handler(
        hummock_options.clone(),
        env,
        hummock_manager_ref,
        worker_node,
        sstable_store.clone(),
    )
    .await;

    let read_version = hummock_event_handler.read_version();

    tokio::spawn(hummock_event_handler.start_hummock_event_handler_worker());

    let hummock_storage = HummockStorage::for_test(
        hummock_options,
        sstable_store,
        hummock_meta_client.clone(),
        read_version,
        event_tx,
    )
    .await
    .unwrap();

    // First batch inserts the anchor and others.
    let mut batch1 = vec![
        (Bytes::from("aa"), StorageValue::new_put("111")),
        (Bytes::from("bb"), StorageValue::new_put("222")),
    ];

    // Make sure the batch is sorted.
    batch1.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

    // Second batch modifies the anchor.
    let mut batch2 = vec![
        (Bytes::from("cc"), StorageValue::new_put("333")),
        (Bytes::from("aa"), StorageValue::new_put("111111")),
    ];

    // Make sure the batch is sorted.
    batch2.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

    // Third batch deletes the anchor
    let mut batch3 = vec![
        (Bytes::from("dd"), StorageValue::new_put("444")),
        (Bytes::from("ee"), StorageValue::new_put("555")),
        (Bytes::from("aa"), StorageValue::new_delete()),
    ];

    // Make sure the batch is sorted.
    batch3.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

    // epoch 0 is reserved by storage service
    let epoch1: u64 = 1;

    // Write the first batch.
    hummock_storage
        .ingest_batch(
            batch1,
            WriteOptions {
                epoch: epoch1,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    // Get the value after flushing to remote.
    let value = hummock_storage
        .get(
            &Bytes::from("aa"),
            epoch1,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value, Bytes::from("111"));
    let value = hummock_storage
        .get(
            &Bytes::from("bb"),
            epoch1,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value, Bytes::from("222"));

    // Test looking for a nonexistent key. `next()` would return the next key.
    let value = hummock_storage
        .get(
            &Bytes::from("ab"),
            epoch1,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(value, None);

    let epoch2 = epoch1 + 1;
    hummock_storage
        .ingest_batch(
            batch2,
            WriteOptions {
                epoch: epoch2,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    // Get the value after flushing to remote.
    let value = hummock_storage
        .get(
            &Bytes::from("aa"),
            epoch2,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value, Bytes::from("111111"));

    // Write the third batch.
    let epoch3 = epoch2 + 1;
    hummock_storage
        .ingest_batch(
            batch3,
            WriteOptions {
                epoch: epoch3,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    // Get the value after flushing to remote.
    let value = hummock_storage
        .get(
            &Bytes::from("aa"),
            epoch3,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(value, None);

    // Get non-existent maximum key.
    let value = hummock_storage
        .get(
            &Bytes::from("ff"),
            epoch3,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(value, None);

    // Write aa bb
    let mut iter = hummock_storage
        .iter(
            (Unbounded, Included(b"ee".to_vec())),
            epoch1,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"aa"[..]),
            Bytes::copy_from_slice(&b"111"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"bb"[..]),
            Bytes::copy_from_slice(&b"222"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(None, iter.next().await.unwrap());

    // Get the anchor value at the first snapshot
    let value = hummock_storage
        .get(
            &Bytes::from("aa"),
            epoch1,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value, Bytes::from("111"));

    // Get the anchor value at the second snapshot
    let value = hummock_storage
        .get(
            &Bytes::from("aa"),
            epoch2,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value, Bytes::from("111111"));
    // Update aa, write cc
    let mut iter = hummock_storage
        .iter(
            (Unbounded, Included(b"ee".to_vec())),
            epoch2,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"aa"[..]),
            Bytes::copy_from_slice(&b"111111"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"bb"[..]),
            Bytes::copy_from_slice(&b"222"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"cc"[..]),
            Bytes::copy_from_slice(&b"333"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(None, iter.next().await.unwrap());

    // Delete aa, write dd,ee
    let mut iter = hummock_storage
        .iter(
            (Unbounded, Included(b"ee".to_vec())),
            epoch3,
            ReadOptions {
                table_id: Default::default(),
                retention_seconds: None,
                check_bloom_filter: true,
                prefix_hint: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"bb"[..]),
            Bytes::copy_from_slice(&b"222"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"cc"[..]),
            Bytes::copy_from_slice(&b"333"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"dd"[..]),
            Bytes::copy_from_slice(&b"444"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(
        Some((
            Bytes::copy_from_slice(&b"ee"[..]),
            Bytes::copy_from_slice(&b"555"[..])
        )),
        iter.next().await.unwrap()
    );
    assert_eq!(None, iter.next().await.unwrap());

    // TODO: add more test cases after sync is supported
}

#[tokio::test]
async fn test_state_store_sync() {
    let sstable_store = mock_sstable_store();
    let hummock_options = Arc::new(default_config_for_test());
    let (env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));

    let (hummock_event_handler, event_tx) = prepare_hummock_event_handler(
        hummock_options.clone(),
        env,
        hummock_manager_ref,
        worker_node,
        sstable_store.clone(),
    )
    .await;

    let read_version = hummock_event_handler.read_version();

    let version_update_notifier_tx = hummock_event_handler.version_update_notifier_tx();
    let epoch1: _ = read_version.read().committed().max_committed_epoch() + 1;

    tokio::spawn(hummock_event_handler.start_hummock_event_handler_worker());

    let hummock_storage = HummockStorage::for_test(
        hummock_options,
        sstable_store,
        hummock_meta_client.clone(),
        read_version,
        event_tx.clone(),
    );

    batch1.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
    hummock_storage
        .ingest_batch(
            batch1,
            WriteOptions {
                epoch: epoch1,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    // ingest 39B batch
    let mut batch2 = vec![
        (
            prefixed_key(Bytes::from("cccc")),
            StorageValue::new_put("3333"),
        ),
        (
            prefixed_key(Bytes::from("dddd")),
            StorageValue::new_put("4444"),
        ),
        (
            prefixed_key(Bytes::from("eeee")),
            StorageValue::new_put("5555"),
        ),
    ];
    batch2.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
    hummock_storage
        .ingest_batch(
            batch2,
            WriteOptions {
                epoch: epoch1,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    let epoch2 = epoch1 + 1;

    // ingest more 13B then will trigger a sync behind the scene
    let mut batch3 = vec![(
        prefixed_key(Bytes::from("eeee")),
        StorageValue::new_put("6666"),
    )];
    batch3.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
    hummock_storage
        .ingest_batch(
            batch3,
            WriteOptions {
                epoch: epoch2,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    let ssts = sync_epoch(&event_tx, epoch1).await.uncommitted_ssts;
    hummock_meta_client
        .commit_epoch(epoch1, ssts)
        .await
        .unwrap();
    try_wait_epoch_for_test(epoch1, version_update_notifier_tx.clone()).await;
    {
        // after sync 1 epoch
        let read_version = hummock_storage.read_version();
        assert_eq!(1, read_version.read().staging().imm.len());
        assert!(read_version.read().staging().sst.is_empty());
    }

    {
        let kv_map = [
            ("aaaa", "1111"),
            ("bbbb", "2222"),
            ("cccc", "3333"),
            ("dddd", "4444"),
            ("eeee", "5555"),
        ];

        for (k, v) in kv_map {
            let value = hummock_storage
                .get(
                    &prefixed_key(k.as_bytes()),
                    epoch1,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(value, Bytes::from(v));
        }
    }

    let ssts = sync_epoch(&event_tx, epoch2).await.uncommitted_ssts;

    hummock_meta_client
        .commit_epoch(epoch2, ssts)
        .await
        .unwrap();
    try_wait_epoch_for_test(epoch2, version_update_notifier_tx.clone()).await;
    {
        // after sync all epoch
        let read_version = hummock_storage.read_version();
        assert!(read_version.read().staging().imm.is_empty());
        assert!(read_version.read().staging().sst.is_empty());
    }

    {
        let kv_map = [
            ("aaaa", "1111"),
            ("bbbb", "2222"),
            ("cccc", "3333"),
            ("dddd", "4444"),
            ("eeee", "6666"),
        ];

        for (k, v) in kv_map {
            let value = hummock_storage
                .get(
                    &prefixed_key(k.as_bytes()),
                    epoch2,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(value, Bytes::from(v));
        }
    }

    // test iter
    {
        let mut iter = hummock_storage
            .iter(
                (Unbounded, Included(prefixed_key(b"eeee").to_vec())),
                epoch1,
                ReadOptions {
                    table_id: Default::default(),
                    retention_seconds: None,
                    check_bloom_filter: true,
                    prefix_hint: None,
                },
            )
            .await
            .unwrap();

        let kv_map = [
            ("aaaa", "1111"),
            ("bbbb", "2222"),
            ("cccc", "3333"),
            ("dddd", "4444"),
            ("eeee", "5555"),
        ];

        for (k, v) in kv_map {
            let result = iter.next().await.unwrap();
            assert_eq!(result, Some((prefixed_key(Bytes::from(k)), Bytes::from(v))));
        }

        assert!(iter.next().await.unwrap().is_none());
    }

    {
        let mut iter = hummock_storage
            .iter(
                (Unbounded, Included(prefixed_key(b"eeee").to_vec())),
                epoch2,
                ReadOptions {
                    table_id: Default::default(),
                    retention_seconds: None,
                    check_bloom_filter: true,
                    prefix_hint: None,
                },
            )
            .await
            .unwrap();

        let kv_map = [
            ("aaaa", "1111"),
            ("bbbb", "2222"),
            ("cccc", "3333"),
            ("dddd", "4444"),
            ("eeee", "6666"),
        ];

        for (k, v) in kv_map {
            let result = iter.next().await.unwrap();
            assert_eq!(result, Some((prefixed_key(Bytes::from(k)), Bytes::from(v))));
        }
    }
}

#[tokio::test]
async fn test_delete_get() {
    let sstable_store = mock_sstable_store();
    let hummock_options = Arc::new(default_config_for_test());
    let (env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));

    let (hummock_event_handler, event_tx) = prepare_hummock_event_handler(
        hummock_options.clone(),
        env,
        hummock_manager_ref,
        worker_node,
        sstable_store.clone(),
    )
    .await;

    let read_version = hummock_event_handler.read_version();
    let version_update_notifier_tx = hummock_event_handler.version_update_notifier_tx();

    tokio::spawn(hummock_event_handler.start_hummock_event_handler_worker());

    let initial_epoch = read_version.read().committed().max_committed_epoch();

    let hummock_storage = HummockStorage::for_test(
        hummock_options,
        sstable_store,
        hummock_meta_client.clone(),
        read_version,
        event_tx.clone(),
    )
    .await
    .unwrap();

    let epoch1 = initial_epoch + 1;
    let batch1 = vec![
        (
            prefixed_key(Bytes::from("aa")),
            StorageValue::new_put("111"),
        ),
        (
            prefixed_key(Bytes::from("bb")),
            StorageValue::new_put("222"),
        ),
    ];
    hummock_storage
        .ingest_batch(
            batch1,
            WriteOptions {
                epoch: epoch1,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    let ssts = sync_epoch(&event_tx, epoch1).await.uncommitted_ssts;
    hummock_meta_client
        .commit_epoch(epoch1, ssts)
        .await
        .unwrap();
    let epoch2 = initial_epoch + 2;
    let batch2 = vec![(prefixed_key(Bytes::from("bb")), StorageValue::new_delete())];
    hummock_storage
        .ingest_batch(
            batch2,
            WriteOptions {
                epoch: epoch2,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();
    let ssts = sync_epoch(&event_tx, epoch2).await.uncommitted_ssts;
    hummock_meta_client
        .commit_epoch(epoch2, ssts)
        .await
        .unwrap();

    try_wait_epoch_for_test(epoch2, version_update_notifier_tx).await;
    assert!(hummock_storage
        .get(
            &prefixed_key("bb".as_bytes()),
            epoch2,
            ReadOptions {
                prefix_hint: None,
                check_bloom_filter: true,
                table_id: Default::default(),
                retention_seconds: None,
            }
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_multiple_epoch_sync() {
    let sstable_store = mock_sstable_store();
    let hummock_options = Arc::new(default_config_for_test());
    let (env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));

    let (hummock_event_handler, event_tx) = prepare_hummock_event_handler(
        hummock_options.clone(),
        env,
        hummock_manager_ref,
        worker_node,
        sstable_store.clone(),
    )
    .await;

    let read_version = hummock_event_handler.read_version();
    let version_update_notifier_tx = hummock_event_handler.version_update_notifier_tx();

    tokio::spawn(hummock_event_handler.start_hummock_event_handler_worker());

    let initial_epoch = read_version.read().committed().max_committed_epoch();

    let hummock_storage = HummockStorage::for_test(
        hummock_options,
        sstable_store,
        hummock_meta_client.clone(),
        read_version,
        event_tx.clone(),
    )
    .await
    .unwrap();

    let epoch1 = initial_epoch + 1;
    let batch1 = vec![
        (
            prefixed_key(Bytes::from("aa")),
            StorageValue::new_put("111"),
        ),
        (
            prefixed_key(Bytes::from("bb")),
            StorageValue::new_put("222"),
        ),
    ];
    hummock_storage
        .ingest_batch(
            batch1,
            WriteOptions {
                epoch: epoch1,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    let epoch2 = initial_epoch + 2;
    let batch2 = vec![(prefixed_key(Bytes::from("bb")), StorageValue::new_delete())];
    hummock_storage
        .ingest_batch(
            batch2,
            WriteOptions {
                epoch: epoch2,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    let epoch3 = initial_epoch + 3;
    let batch3 = vec![
        (
            prefixed_key(Bytes::from("aa")),
            StorageValue::new_put("444"),
        ),
        (
            prefixed_key(Bytes::from("bb")),
            StorageValue::new_put("555"),
        ),
    ];
    hummock_storage
        .ingest_batch(
            batch3,
            WriteOptions {
                epoch: epoch3,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();
    let test_get = || {
        let hummock_storage_clone = hummock_storage.clone();
        async move {
            assert_eq!(
                hummock_storage_clone
                    .get(
                        &prefixed_key("bb".as_bytes()),
                        epoch1,
                        ReadOptions {
                            table_id: Default::default(),
                            retention_seconds: None,
                            check_bloom_filter: true,
                            prefix_hint: None,
                        },
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                "222".as_bytes()
            );
            assert!(hummock_storage_clone
                .get(
                    &prefixed_key("bb".as_bytes()),
                    epoch2,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                hummock_storage_clone
                    .get(
                        &prefixed_key("bb".as_bytes()),
                        epoch3,
                        ReadOptions {
                            table_id: Default::default(),
                            retention_seconds: None,
                            check_bloom_filter: true,
                            prefix_hint: None,
                        },
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                "555".as_bytes()
            );
        }
    };
    test_get().await;
    event_tx
        .send(HummockEvent::SealEpoch {
            epoch: epoch1,
            is_checkpoint: false,
        })
        .unwrap();
    let sync_result2 = sync_epoch(&event_tx, epoch2).await;
    let sync_result3 = sync_epoch(&event_tx, epoch3).await;
    test_get().await;
    hummock_meta_client
        .commit_epoch(epoch2, sync_result2.uncommitted_ssts)
        .await
        .unwrap();
    hummock_meta_client
        .commit_epoch(epoch3, sync_result3.uncommitted_ssts)
        .await
        .unwrap();

    try_wait_epoch_for_test(epoch3, version_update_notifier_tx).await;
    test_get().await;
}

#[tokio::test]
async fn test_iter_with_min_epoch() {
    let sstable_store = mock_sstable_store();
    let hummock_options = Arc::new(default_config_for_test());
    let (env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));

    let (hummock_event_handler, event_tx) = prepare_hummock_event_handler(
        hummock_options.clone(),
        env,
        hummock_manager_ref,
        worker_node,
        sstable_store.clone(),
    )
    .await;

    let read_version = hummock_event_handler.read_version();
    let version_update_notifier_tx = hummock_event_handler.version_update_notifier_tx();

    tokio::spawn(hummock_event_handler.start_hummock_event_handler_worker());

    let hummock_storage = HummockStorage::for_test(
        hummock_options,
        sstable_store,
        hummock_meta_client.clone(),
        read_version,
        event_tx.clone(),
    )
    .await
    .unwrap();

    let epoch1 = (31 * 1000) << 16;

    let gen_key = |index: usize| -> String { format!("key_{}", index) };

    let gen_val = |index: usize| -> String { format!("val_{}", index) };

    // epoch 1 write
    let batch_epoch1: Vec<(Bytes, StorageValue)> = (0..10)
        .into_iter()
        .map(|index| {
            (
                prefixed_key(Bytes::from(gen_key(index))),
                StorageValue::new_put(gen_val(index)),
            )
        })
        .collect();

    hummock_storage
        .ingest_batch(
            batch_epoch1,
            WriteOptions {
                epoch: epoch1,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    let epoch2 = (32 * 1000) << 16;
    // epoch 2 write
    let batch_epoch2: Vec<(Bytes, StorageValue)> = (20..30)
        .into_iter()
        .map(|index| {
            (
                prefixed_key(Bytes::from(gen_key(index))),
                StorageValue::new_put(gen_val(index)),
            )
        })
        .collect();

    hummock_storage
        .ingest_batch(
            batch_epoch2,
            WriteOptions {
                epoch: epoch2,
                table_id: Default::default(),
            },
        )
        .await
        .unwrap();

    {
        // test before sync
        {
            let iter = hummock_storage
                .iter(
                    (Unbounded, Unbounded),
                    epoch1,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap();

            let result = iter.collect(None).await.unwrap();
            assert_eq!(10, result.len());
        }

        {
            let iter = hummock_storage
                .iter(
                    (Unbounded, Unbounded),
                    epoch2,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap();

            let result = iter.collect(None).await.unwrap();
            assert_eq!(20, result.len());
        }

        {
            let iter = hummock_storage
                .iter(
                    (Unbounded, Unbounded),
                    epoch2,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: Some(1),
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap();

            let result = iter.collect(None).await.unwrap();
            assert_eq!(10, result.len());
        }
    }

    {
        // test after sync

        let sync_result1 = sync_epoch(&event_tx, epoch1).await;
        let sync_result2 = sync_epoch(&event_tx, epoch2).await;
        hummock_meta_client
            .commit_epoch(epoch1, sync_result1.uncommitted_ssts)
            .await
            .unwrap();
        hummock_meta_client
            .commit_epoch(epoch2, sync_result2.uncommitted_ssts)
            .await
            .unwrap();

        try_wait_epoch_for_test(epoch2, version_update_notifier_tx).await;

        {
            let iter = hummock_storage
                .iter(
                    (Unbounded, Unbounded),
                    epoch1,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap();

            let result = iter.collect(None).await.unwrap();
            assert_eq!(10, result.len());
        }

        {
            let iter = hummock_storage
                .iter(
                    (Unbounded, Unbounded),
                    epoch2,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: None,
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap();

            let result = iter.collect(None).await.unwrap();
            assert_eq!(20, result.len());
        }

        {
            let iter = hummock_storage
                .iter(
                    (Unbounded, Unbounded),
                    epoch2,
                    ReadOptions {
                        table_id: Default::default(),
                        retention_seconds: Some(1),
                        check_bloom_filter: true,
                        prefix_hint: None,
                    },
                )
                .await
                .unwrap();

            let result = iter.collect(None).await.unwrap();
            assert_eq!(10, result.len());
        }
    }
}
