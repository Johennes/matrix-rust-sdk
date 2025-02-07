// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The event cache is an abstraction layer, sitting between the Rust SDK and a
//! final client, that acts as a global observer of all the rooms, gathering and
//! inferring some extra useful information about each room. In particular, this
//! doesn't require subscribing to a specific room to get access to this
//! information.
//!
//! It's intended to be fast, robust and easy to maintain.
//!
//! See the [github issue](https://github.com/matrix-org/matrix-rust-sdk/issues/3058) for more details about the historical reasons that led us to start writing this.
//!
//! Most of it is still a work-in-progress, as of 2024-01-22.
//!
//! The desired set of features it may eventually implement is the following:
//!
//! - [ ] compute proper unread room counts, and use backpagination to get
//!   missing messages/notifications/mentions, if needs be.
//! - [ ] expose that information with a new data structure similar to the
//!   `RoomInfo`, and that may update a `RoomListService`.
//! - [ ] provide read receipts for each message.
//! - [ ] backwards and forward pagination, and reconcile results with cached
//!   timelines.
//! - [ ] retry decryption upon receiving new keys (from an encryption sync
//!   service or from a key backup).
//! - [ ] expose the latest event for a given room.
//! - [ ] caching of events on-disk.

#![forbid(missing_docs)]

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, OnceLock, Weak},
};

use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, RoomUpdates, Timeline},
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    events::{AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    OwnedEventId, OwnedRoomId, RoomId,
};
use tokio::sync::{
    broadcast::{error::RecvError, Receiver, Sender},
    Mutex, RwLock,
};
use tracing::{error, instrument, trace, warn};

use self::store::{EventCacheStore, MemoryStore};
use crate::{client::ClientInner, Client, Room};

mod store;

/// An error observed in the [`EventCache`].
#[derive(thiserror::Error, Debug)]
pub enum EventCacheError {
    /// The [`EventCache`] instance hasn't been initialized with
    /// [`EventCache::subscribe`]
    #[error(
        "The EventCache hasn't subscribed to sync responses yet, call `EventCache::subscribe()`"
    )]
    NotSubscribedYet,

    /// The [`EventCache`] owns a weak reference to the [`Client`] it pertains
    /// to. It's possible this weak reference points to nothing anymore, at
    /// times where we try to use the client.
    #[error("The owning client of the event cache has been dropped.")]
    ClientDropped,
}

/// A result using the [`EventCacheError`].
pub type Result<T> = std::result::Result<T, EventCacheError>;

/// Hold handles to the tasks spawn by a [`RoomEventCache`].
pub struct EventCacheDropHandles {
    listen_updates_task: JoinHandle<()>,
}

impl Debug for EventCacheDropHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCacheDropHandles").finish_non_exhaustive()
    }
}

impl Drop for EventCacheDropHandles {
    fn drop(&mut self) {
        self.listen_updates_task.abort();
    }
}

/// An event cache, providing lots of useful functionality for clients.
///
/// Cloning is shallow, and thus is cheap to do.
///
/// See also the module-level comment.
#[derive(Clone)]
pub struct EventCache {
    /// Reference to the inner cache.
    inner: Arc<EventCacheInner>,
}

impl Debug for EventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCache").finish_non_exhaustive()
    }
}

impl EventCache {
    /// Create a new [`EventCache`] for the given client.
    pub(crate) fn new(client: &Arc<ClientInner>) -> Self {
        let store = Arc::new(MemoryStore::new());
        let inner = Arc::new(EventCacheInner {
            client: Arc::downgrade(client),
            by_room: Default::default(),
            store,
            process_lock: Default::default(),
            drop_handles: Default::default(),
        });

        Self { inner }
    }

    /// Starts subscribing the [`EventCache`] to sync responses, if not done
    /// before.
    ///
    /// Re-running this has no effect if we already subscribed before, and is
    /// cheap.
    pub fn subscribe(&self) -> Result<()> {
        let client = self.inner.client()?;

        let _ = self.inner.drop_handles.get_or_init(|| {
            // Spawn the task that will listen to all the room updates at once.
            let room_updates_feed = client.subscribe_to_all_room_updates();
            let listen_updates_task =
                spawn(Self::listen_task(self.inner.clone(), room_updates_feed));

            Arc::new(EventCacheDropHandles { listen_updates_task })
        });

        Ok(())
    }

    async fn listen_task(
        inner: Arc<EventCacheInner>,
        mut room_updates_feed: Receiver<RoomUpdates>,
    ) {
        trace!("Spawning the listen task");
        loop {
            match room_updates_feed.recv().await {
                Ok(updates) => {
                    if let Err(err) = inner.handle_room_updates(updates).await {
                        match err {
                            EventCacheError::ClientDropped => {
                                // The client has dropped, exit the listen task.
                                break;
                            }
                            err => {
                                error!("Error when handling room updates: {err}");
                            }
                        }
                    }
                }

                Err(RecvError::Lagged(_)) => {
                    // Forget everything we know; we could have missed events, and we have
                    // no way to reconcile at the moment!
                    // TODO: implement Smart Matching™,
                    let mut by_room = inner.by_room.write().await;
                    for room_id in by_room.keys() {
                        if let Err(err) = inner.store.clear_room_events(room_id).await {
                            error!("unable to clear room after room updates lag: {err}");
                        }
                    }
                    by_room.clear();
                }

                Err(RecvError::Closed) => {
                    // The sender has shut down, exit.
                    break;
                }
            }
        }
    }

    /// Return a room-specific view over the [`EventCache`].
    pub(crate) async fn for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(Option<RoomEventCache>, Arc<EventCacheDropHandles>)> {
        let Some(drop_handles) = self.inner.drop_handles.get().cloned() else {
            return Err(EventCacheError::NotSubscribedYet);
        };

        let room = self.inner.for_room(room_id).await?;

        Ok((room, drop_handles))
    }

    /// Add an initial set of events to the event cache, reloaded from a cache.
    ///
    /// TODO: temporary for API compat, as the event cache should take care of
    /// its own store.
    #[instrument(skip(self, events))]
    pub async fn add_initial_events(
        &self,
        room_id: &RoomId,
        events: Vec<SyncTimelineEvent>,
    ) -> Result<()> {
        let Some(room_cache) = self.inner.for_room(room_id).await? else {
            warn!("unknown room, skipping");
            return Ok(());
        };

        // We could have received events during a previous sync; remove them all, since
        // we can't know where to insert the "initial events" with respect to
        // them.
        self.inner.store.clear_room_events(room_id).await?;
        let _ = room_cache.inner.sender.send(RoomEventCacheUpdate::Clear);

        room_cache.inner.append_events(events).await?;

        Ok(())
    }
}

struct EventCacheInner {
    /// A weak reference to the inner client, useful when trying to get a handle
    /// on the owning client.
    client: Weak<ClientInner>,

    /// Lazily-filled cache of live [`RoomEventCache`], once per room.
    by_room: RwLock<BTreeMap<OwnedRoomId, RoomEventCache>>,

    /// Backend used for storage.
    store: Arc<dyn EventCacheStore>,

    /// A lock to make sure that despite multiple updates coming to the
    /// `EventCache`, it will only handle one at a time.
    ///
    /// [`Mutex`] is “fair”, as it is implemented as a FIFO. It is important to
    /// ensure that multiple updates will be applied in the correct order.
    process_lock: Mutex<()>,

    /// Handles to keep alive the task listening to updates.
    drop_handles: OnceLock<Arc<EventCacheDropHandles>>,
}

impl EventCacheInner {
    fn client(&self) -> Result<Client> {
        Ok(Client { inner: self.client.upgrade().ok_or(EventCacheError::ClientDropped)? })
    }

    /// Handles a single set of room updates at once.
    #[instrument(skip(self, updates))]
    async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        // First, take the lock that indicates we're processing updates, to avoid
        // handling multiple updates concurrently.
        let _process_lock = self.process_lock.lock().await;

        // Left rooms.
        for (room_id, left_room_update) in updates.leave {
            let Some(room) = self.for_room(&room_id).await? else {
                warn!(%room_id, "missing left room");
                continue;
            };

            if let Err(err) = room.inner.handle_left_room_update(left_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!("handling left room update: {err}");
            }
        }

        // Joined rooms.
        for (room_id, joined_room_update) in updates.join {
            let Some(room) = self.for_room(&room_id).await? else {
                warn!(%room_id, "missing joined room");
                continue;
            };

            if let Err(err) = room.inner.handle_joined_room_update(joined_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!("handling joined room update: {err}");
            }
        }

        // Invited rooms.
        // TODO: we don't anything with `updates.invite` at this point.

        Ok(())
    }

    /// Return a room-specific view over the [`EventCache`].
    ///
    /// It may not be found, if the room isn't known to the client, in which
    /// case it'll return None.
    async fn for_room(&self, room_id: &RoomId) -> Result<Option<RoomEventCache>> {
        // Fast path: the entry exists; let's acquire a read lock, it's cheaper than a
        // write lock.
        let by_room_guard = self.by_room.read().await;

        match by_room_guard.get(room_id) {
            Some(room) => Ok(Some(room.clone())),

            None => {
                // Slow-path: the entry doesn't exist; let's acquire a write lock.
                drop(by_room_guard);
                let mut by_room_guard = self.by_room.write().await;

                // In the meanwhile, some other caller might have obtained write access and done
                // the same, so check for existence again.
                if let Some(room) = by_room_guard.get(room_id) {
                    return Ok(Some(room.clone()));
                }

                let Some(room) = self.client()?.get_room(room_id) else {
                    return Ok(None);
                };

                let room_event_cache = RoomEventCache::new(room, self.store.clone());

                by_room_guard.insert(room_id.to_owned(), room_event_cache.clone());

                Ok(Some(room_event_cache))
            }
        }
    }
}

/// A subset of an event cache, for a room.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct RoomEventCache {
    inner: Arc<RoomEventCacheInner>,
}

impl Debug for RoomEventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomEventCache").finish_non_exhaustive()
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    fn new(room: Room, store: Arc<dyn EventCacheStore>) -> Self {
        Self { inner: Arc::new(RoomEventCacheInner::new(room, store)) }
    }

    /// Subscribe to room updates for this room, after getting the initial list
    /// of events. XXX: Could/should it use some kind of `Observable`
    /// instead? Or not something async, like explicit handlers as our event
    /// handlers?
    pub async fn subscribe(
        &self,
    ) -> Result<(Vec<SyncTimelineEvent>, Receiver<RoomEventCacheUpdate>)> {
        Ok((
            self.inner.store.room_events(self.inner.room.room_id()).await?,
            self.inner.sender.subscribe(),
        ))
    }
}

/// The (non-clonable) details of the `RoomEventCache`.
struct RoomEventCacheInner {
    /// Sender part for subscribers to this room.
    sender: Sender<RoomEventCacheUpdate>,

    /// A pointer to the store implementation used for this event cache.
    store: Arc<dyn EventCacheStore>,

    /// The Client [`Room`] this event cache pertains to.
    room: Room,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(room: Room, store: Arc<dyn EventCacheStore>) -> Self {
        let sender = Sender::new(32);
        Self { room, store, sender }
    }

    async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        self.handle_timeline(
            updates.timeline,
            updates.ephemeral.clone(),
            updates.account_data,
            updates.ambiguity_changes,
        )
        .await?;
        Ok(())
    }

    async fn handle_timeline(
        &self,
        timeline: Timeline,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if timeline.limited {
            // Ideally we'd try to reconcile existing events against those received in the
            // timeline, but we're not there yet. In the meanwhile, clear the
            // items from the room. TODO: implement Smart Matching™.
            trace!("limited timeline, clearing all previous events");
            self.store.clear_room_events(self.room.room_id()).await?;
            let _ = self.sender.send(RoomEventCacheUpdate::Clear);
        }

        // Add all the events to the backend.
        if !timeline.events.is_empty()
            || timeline.prev_batch.is_some()
            || !ephemeral.is_empty()
            || !account_data.is_empty()
            || !ambiguity_changes.is_empty()
        {
            trace!("adding new events");
            self.store.add_room_events(self.room.room_id(), timeline.events.clone()).await?;

            // Propagate events to observers.
            let _ = self.sender.send(RoomEventCacheUpdate::Append {
                events: timeline.events,
                prev_batch: timeline.prev_batch,
                ephemeral,
                account_data,
                ambiguity_changes,
            });
        }

        Ok(())
    }

    async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline, Vec::new(), Vec::new(), updates.ambiguity_changes)
            .await?;
        Ok(())
    }

    /// Append a set of events to the room cache and storage, notifying
    /// observers.
    async fn append_events(&self, events: Vec<SyncTimelineEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        self.store.add_room_events(self.room.room_id(), events.clone()).await?;

        let _ = self.sender.send(RoomEventCacheUpdate::Append {
            events,
            prev_batch: None,
            account_data: Default::default(),
            ephemeral: Default::default(),
            ambiguity_changes: Default::default(),
        });

        Ok(())
    }
}

/// An update related to events happened in a room.
#[derive(Debug, Clone)]
pub enum RoomEventCacheUpdate {
    /// The room has been cleared from events.
    Clear,
    /// The room has new events.
    Append {
        /// All the new events that have been added to the room.
        events: Vec<SyncTimelineEvent>,
        /// XXX: this is temporary, until backpagination lives in the event
        /// cache.
        prev_batch: Option<String>,
        /// XXX: this is temporary, until account data lives in the event cache
        /// — or will it live there?
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        /// XXX: this is temporary, until read receipts are handled in the event
        /// cache
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        /// Collection of ambiguity changes that room member events trigger.
        ///
        /// This is a map of event ID of the `m.room.member` event to the
        /// details of the ambiguity change.
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    },
}
