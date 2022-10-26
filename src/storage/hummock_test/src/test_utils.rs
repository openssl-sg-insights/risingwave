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
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use bytes::{BufMut, BufMut, Bytes, Bytes};
use parking_lot::RwLock;
use risingwave_common::config::StorageConfig;
use risingwave_common::error::Result;
use risingwave_common::util::addr::HostAddr;
use risingwave_common_service::observer_manager::{Channel, NotificationClient, ObserverManager};
use risingwave_hummock_sdk::filter_key_extractor::FilterKeyExtractorManager;
use risingwave_meta::hummock::{HummockManager, HummockManagerRef};
use risingwave_meta::manager::{MessageStatus, MetaSrvEnv, NotificationManagerRef, WorkerKey};
use risingwave_meta::storage::{MemStore, MetaStore};
use risingwave_pb::common::WorkerNode;
use risingwave_pb::hummock::pin_version_response;
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::meta::{MetaSnapshot, SubscribeResponse, SubscribeType};
use risingwave_storage::hummock::event_handler::HummockEvent;
use risingwave_storage::hummock::local_version::pinned_version::PinnedVersion;
use risingwave_storage::hummock::observer_manager::HummockObserverNode;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub struct TestNotificationClient<S: MetaStore> {
    addr: HostAddr,
    notification_manager: NotificationManagerRef<S>,
    hummock_manager: HummockManagerRef<S>,
}

pub struct TestChannel<T>(UnboundedReceiver<std::result::Result<T, MessageStatus>>);

#[async_trait::async_trait]
impl<T: Send + 'static> Channel for TestChannel<T> {
    type Item = T;

    async fn message(&mut self) -> std::result::Result<Option<T>, MessageStatus> {
        match self.0.recv().await {
            None => Ok(None),
            Some(result) => result.map(|r| Some(r)),
        }
    }
}

impl<S: MetaStore> TestNotificationClient<S> {
    pub fn new(
        addr: HostAddr,
        notification_manager: NotificationManagerRef<S>,
        hummock_manager: HummockManagerRef<S>,
    ) -> Self {
        Self {
            addr,
            notification_manager,
            hummock_manager,
        }
    }
}

#[async_trait::async_trait]
impl<S: MetaStore> NotificationClient for TestNotificationClient<S> {
    type Channel = TestChannel<SubscribeResponse>;

    async fn subscribe(&self, subscribe_type: SubscribeType) -> Result<Self::Channel> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let hummock_manager_guard = self.hummock_manager.get_read_guard().await;
        let meta_snapshot = MetaSnapshot {
            hummock_version: Some(hummock_manager_guard.current_version.clone()),
            ..Default::default()
        };
        tx.send(Ok(SubscribeResponse {
            status: None,
            operation: Operation::Snapshot as i32,
            info: Some(Info::Snapshot(meta_snapshot)),
            version: self.notification_manager.current_version().await,
        }))
        .unwrap();
        self.notification_manager
            .insert_sender(subscribe_type, WorkerKey(self.addr.to_protobuf()), tx)
            .await;
        Ok(TestChannel(rx))
    }
}

pub fn get_test_notification_client(
    env: MetaSrvEnv<MemStore>,
    hummock_manager_ref: Arc<HummockManager<MemStore>>,
    worker_node: WorkerNode,
) -> TestNotificationClient<MemStore> {
    TestNotificationClient::new(
        worker_node.get_host().unwrap().into(),
        env.notification_manager_ref(),
        hummock_manager_ref,
    )
}

pub async fn prepare_first_valid_version(
    env: MetaSrvEnv<MemStore>,
    hummock_manager_ref: HummockManagerRef<MemStore>,
    worker_node: WorkerNode,
) -> (
    PinnedVersion,
    UnboundedSender<HummockEvent>,
    UnboundedReceiver<HummockEvent>,
) {
    let (tx, mut rx) = unbounded_channel();
    let notification_client =
        get_test_notification_client(env, hummock_manager_ref.clone(), worker_node.clone());
    let observer_manager = ObserverManager::new(
        notification_client,
        HummockObserverNode::new(Arc::new(FilterKeyExtractorManager::default()), tx.clone()),
    )
    .await;
    let _ = observer_manager.start().await.unwrap();
    let hummock_version = match rx.recv().await {
        Some(HummockEvent::VersionUpdate(pin_version_response::Payload::PinnedVersion(
            version,
        ))) => version,
        _ => unreachable!("should be full version"),
    };

    (
        PinnedVersion::new(hummock_version, unbounded_channel().0),
        tx,
        rx,
    )
}

/// Prefix the `key` with a dummy table id.
/// We use `0` because：
/// - This value is used in the code to identify unit tests and prevent some parameters that are not
///   easily constructible in tests from breaking the test.
/// - When calling state store interfaces, we normally pass `TableId::default()`, which is `0`.
pub fn prefixed_key<T: AsRef<[u8]>>(key: T) -> Bytes {
    let mut buf = Vec::new();
    buf.put_u32(0);
    buf.put_slice(key.as_ref());
    buf.into()
}
