// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2024 Rust Nostr Developers
// Distributed under the MIT software license

//! Nostr Database Indexes

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use nostr::event::id;
use nostr::nips::nip01::Coordinate;
use nostr::secp256k1::XOnlyPublicKey;
use nostr::{Alphabet, Event, EventId, Filter, GenericTagValue, Kind, Timestamp};
use rayon::prelude::*;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::raw::RawEvent;
use crate::tag_indexes::{TagIndexValues, TagIndexes};

/// Public Key Prefix Size
const PUBLIC_KEY_PREFIX_SIZE: usize = 8;

#[derive(Debug, Error)]
enum Error {
    #[error(transparent)]
    EventId(#[from] id::Error),
}

/// Event Index
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventIndex {
    /// Timestamp (seconds)
    created_at: Timestamp,
    /// Event ID
    event_id: EventId,
    /// Public key prefix
    pubkey: PublicKeyPrefix,
    /// Kind
    kind: Kind,
    /// Tag indexes
    tags: TagIndexes,
}

impl PartialOrd for EventIndex {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EventIndex {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.created_at != other.created_at {
            self.created_at.cmp(&other.created_at).reverse()
        } else {
            self.event_id.cmp(&other.event_id)
        }
    }
}

impl TryFrom<RawEvent> for EventIndex {
    type Error = nostr::event::id::Error;
    fn try_from(raw: RawEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            created_at: raw.created_at,
            event_id: EventId::from_slice(&raw.id)?,
            pubkey: PublicKeyPrefix::from(raw.pubkey),
            kind: raw.kind,
            tags: TagIndexes::from(raw.tags.into_iter()),
        })
    }
}

impl From<&Event> for EventIndex {
    fn from(e: &Event) -> Self {
        Self {
            created_at: e.created_at,
            event_id: e.id,
            pubkey: PublicKeyPrefix::from(e.pubkey),
            kind: e.kind,
            tags: TagIndexes::from(e.tags.iter().map(|t| t.as_vec())),
        }
    }
}

/// Public Key prefix
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PublicKeyPrefix([u8; PUBLIC_KEY_PREFIX_SIZE]);

impl From<&XOnlyPublicKey> for PublicKeyPrefix {
    fn from(pk: &XOnlyPublicKey) -> Self {
        let pk: [u8; 32] = pk.serialize();
        Self::from(pk)
    }
}

impl From<XOnlyPublicKey> for PublicKeyPrefix {
    fn from(pk: XOnlyPublicKey) -> Self {
        Self::from(&pk)
    }
}

impl From<[u8; 32]> for PublicKeyPrefix {
    fn from(pk: [u8; 32]) -> Self {
        let mut pubkey = [0u8; PUBLIC_KEY_PREFIX_SIZE];
        pubkey.copy_from_slice(&pk[..PUBLIC_KEY_PREFIX_SIZE]);
        Self(pubkey)
    }
}

#[derive(Default)]
struct FilterIndex {
    ids: HashSet<EventId>,
    authors: HashSet<PublicKeyPrefix>,
    kinds: HashSet<Kind>,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    generic_tags: HashMap<Alphabet, BTreeSet<GenericTagValue>>,
}

impl FilterIndex {
    fn author(mut self, author: PublicKeyPrefix) -> Self {
        self.authors.insert(author);
        self
    }

    fn kind(mut self, kind: Kind) -> Self {
        self.kinds.insert(kind);
        self
    }

    fn identifier<S>(mut self, identifier: S) -> Self
    where
        S: Into<String>,
    {
        let identifier: GenericTagValue = GenericTagValue::String(identifier.into());
        self.generic_tags
            .entry(Alphabet::D)
            .and_modify(|list| {
                list.insert(identifier.clone());
            })
            .or_default()
            .insert(identifier);
        self
    }

    fn ids_match(&self, event: &EventIndex) -> bool {
        self.ids.is_empty() || self.ids.contains(&event.event_id)
    }

    fn authors_match(&self, event: &EventIndex) -> bool {
        self.authors.is_empty() || self.authors.contains(&event.pubkey)
    }

    fn tag_match(&self, event: &EventIndex) -> bool {
        if self.generic_tags.is_empty() {
            return true;
        }
        if event.tags.is_empty() {
            return false;
        }

        self.generic_tags.iter().all(|(tagname, set)| {
            event.tags.get(tagname).map_or(false, |valset| {
                TagIndexValues::iter(set.iter())
                    .filter(|t| valset.contains(t))
                    .count()
                    > 0
            })
        })
    }

    fn kind_match(&self, kind: &Kind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(kind)
    }

    pub fn match_event(&self, event: &EventIndex) -> bool {
        self.ids_match(event)
            && self.since.map_or(true, |t| event.created_at >= t)
            && self.until.map_or(true, |t| event.created_at <= t)
            && self.kind_match(&event.kind)
            && self.authors_match(event)
            && self.tag_match(event)
    }
}

impl From<Filter> for FilterIndex {
    fn from(value: Filter) -> Self {
        Self {
            ids: value.ids.into_iter().collect(),
            authors: value
                .authors
                .into_iter()
                .map(PublicKeyPrefix::from)
                .collect(),
            kinds: value.kinds.into_iter().collect(),
            since: value.since,
            until: value.until,
            generic_tags: value.generic_tags.into_iter().collect(),
        }
    }
}

/// Event Index Result
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventIndexResult {
    /// Handled event should be stored into database?
    pub to_store: bool,
    /// List of events that should be removed from database
    pub to_discard: HashSet<EventId>,
}

/// Database Indexes
#[derive(Debug, Clone, Default)]
pub struct DatabaseIndexes {
    index: Arc<RwLock<BTreeSet<EventIndex>>>,
    deleted_ids: Arc<RwLock<HashSet<EventId>>>,
    deleted_coordinates: Arc<RwLock<HashMap<Coordinate, Timestamp>>>,
}

impl DatabaseIndexes {
    /// New empty indexes
    pub fn new() -> Self {
        Self::default()
    }

    /// Bulk index
    #[tracing::instrument(skip_all)]
    pub async fn bulk_index(&self, events: BTreeSet<RawEvent>) -> HashSet<EventId> {
        let mut index = self.index.write().await;
        let mut deleted_ids = self.deleted_ids.write().await;
        let mut deleted_coordinates = self.deleted_coordinates.write().await;

        let mut to_discard: HashSet<EventId> = HashSet::new();
        let now = Timestamp::now();

        events
            .into_iter()
            .filter(|raw| !raw.kind.is_ephemeral())
            .for_each(|event| {
                let _ = self.index_raw_event(
                    &mut index,
                    &mut deleted_ids,
                    &mut deleted_coordinates,
                    &mut to_discard,
                    event,
                    &now,
                );
            });

        // Remove events
        if !to_discard.is_empty() {
            index.retain(|e| !to_discard.contains(&e.event_id));
            deleted_ids.par_extend(to_discard.par_iter());
        }

        to_discard
    }

    fn index_raw_event(
        &self,
        index: &mut BTreeSet<EventIndex>,
        deleted_ids: &mut HashSet<EventId>,
        deleted_coordinates: &mut HashMap<Coordinate, Timestamp>,
        to_discard: &mut HashSet<EventId>,
        raw: RawEvent,
        now: &Timestamp,
    ) -> Result<(), Error> {
        // Parse event ID
        let event_id: EventId = EventId::from_slice(&raw.id)?;

        // Check if was deleted
        if deleted_ids.contains(&event_id) {
            return Ok(());
        }

        // Check if is expired
        if raw.is_expired(now) {
            to_discard.insert(event_id);
            return Ok(());
        }

        // Compose others fields
        let pubkey_prefix: PublicKeyPrefix = PublicKeyPrefix::from(raw.pubkey);

        let mut should_insert: bool = true;

        if raw.kind.is_replaceable() {
            let filter: FilterIndex = FilterIndex::default().author(pubkey_prefix).kind(raw.kind);
            for ev in self.internal_query(index, deleted_ids, filter) {
                if ev.created_at > raw.created_at {
                    should_insert = false;
                } else if ev.created_at <= raw.created_at {
                    to_discard.insert(ev.event_id);
                }
            }
        } else if raw.kind.is_parameterized_replaceable() {
            match raw.identifier() {
                Some(identifier) => {
                    let filter: FilterIndex = FilterIndex::default()
                        .author(pubkey_prefix)
                        .kind(raw.kind)
                        .identifier(identifier);
                    for ev in self.internal_query(index, deleted_ids, filter) {
                        if ev.created_at >= raw.created_at {
                            should_insert = false;
                        } else if ev.created_at < raw.created_at {
                            to_discard.insert(ev.event_id);
                        }
                    }
                }
                None => should_insert = false,
            }
        } else if raw.kind == Kind::EventDeletion {
            // Check `e` tags
            let ids = raw.event_ids();
            let filter: Filter = Filter::new().ids(ids).until(raw.created_at);
            if !filter.ids.is_empty() {
                to_discard.par_extend(
                    self.internal_parallel_query(index, deleted_ids, filter)
                        .filter(|ev| ev.pubkey == pubkey_prefix)
                        .map(|ev| ev.event_id),
                );
            }

            // Check `a` tags
            for coordinate in raw.coordinates() {
                let coordinate_pubkey_prefix: PublicKeyPrefix =
                    PublicKeyPrefix::from(coordinate.pubkey);
                if coordinate_pubkey_prefix == pubkey_prefix {
                    // Save deleted coordinate at certain timestamp
                    deleted_coordinates.insert(coordinate.clone(), raw.created_at);

                    let filter: Filter = coordinate.into();
                    let filter: Filter = filter.until(raw.created_at);
                    // Not check if ev.pubkey match the pubkey_prefix because asume that query
                    // returned only the events owned by pubkey_prefix
                    to_discard.par_extend(
                        self.internal_parallel_query(index, deleted_ids, filter)
                            .map(|ev| ev.event_id),
                    );
                }
            }
        }

        // Insert event
        if should_insert {
            index.insert(EventIndex {
                created_at: raw.created_at,
                event_id,
                pubkey: pubkey_prefix,
                kind: raw.kind,
                tags: TagIndexes::from(raw.tags.into_iter()),
            });
        }

        Ok(())
    }

    /// Index [`Event`]
    ///
    /// **This method assume that [`Event`] was already verified**
    #[tracing::instrument(skip_all, level = "trace")]
    pub async fn index_event(&self, event: &Event) -> EventIndexResult {
        // Check if it's expired or ephemeral
        if event.is_expired() || event.is_ephemeral() {
            return EventIndexResult::default();
        }

        // Acquire write lock
        let mut index = self.index.write().await;
        let mut deleted_ids = self.deleted_ids.write().await;
        let mut deleted_coordinates = self.deleted_coordinates.write().await;

        let mut should_insert: bool = true;
        let mut to_discard: HashSet<EventId> = HashSet::new();

        // Check if was deleted
        if deleted_ids.contains(&event.id) {
            to_discard.insert(event.id);
            return EventIndexResult {
                to_store: false,
                to_discard,
            };
        }

        if event.is_replaceable() {
            let filter: Filter = Filter::new().author(event.pubkey).kind(event.kind);
            for ev in self.internal_query(&index, &deleted_ids, filter) {
                if ev.created_at > event.created_at {
                    should_insert = false;
                } else if ev.created_at <= event.created_at {
                    to_discard.insert(ev.event_id);
                }
            }
        } else if event.is_parameterized_replaceable() {
            match event.identifier() {
                Some(identifier) => {
                    let filter: Filter = Filter::new()
                        .author(event.pubkey)
                        .kind(event.kind)
                        .identifier(identifier);
                    for ev in self.internal_query(&index, &deleted_ids, filter) {
                        if ev.created_at >= event.created_at {
                            should_insert = false;
                        } else if ev.created_at < event.created_at {
                            to_discard.insert(ev.event_id);
                        }
                    }
                }
                None => should_insert = false,
            }
        } else if event.kind == Kind::EventDeletion {
            let pubkey_prefix: PublicKeyPrefix = PublicKeyPrefix::from(event.pubkey);

            // Check `e` tags
            let ids = event.event_ids().copied();
            let filter: Filter = Filter::new().ids(ids).until(event.created_at);
            if !filter.ids.is_empty() {
                to_discard.par_extend(
                    self.internal_parallel_query(&index, &deleted_ids, filter)
                        .filter(|ev| ev.pubkey == pubkey_prefix)
                        .map(|ev| ev.event_id),
                );
            }

            // Check `a` tags
            for coordinate in event.coordinates() {
                let coordinate_pubkey_prefix: PublicKeyPrefix =
                    PublicKeyPrefix::from(coordinate.pubkey);
                if coordinate_pubkey_prefix == pubkey_prefix {
                    // Save deleted coordinate at certain timestamp
                    deleted_coordinates.insert(coordinate.clone(), event.created_at);

                    let filter: Filter = coordinate.into();
                    let filter: Filter = filter.until(event.created_at);
                    to_discard.par_extend(
                        self.internal_parallel_query(&index, &deleted_ids, filter)
                            .map(|ev| ev.event_id),
                    );
                }
            }
        }

        // Remove events
        if !to_discard.is_empty() {
            index.retain(|e| !to_discard.contains(&e.event_id));
            deleted_ids.par_extend(to_discard.par_iter());
        }

        // Insert event
        if should_insert {
            index.insert(EventIndex::from(event));
        }

        EventIndexResult {
            to_store: should_insert,
            to_discard,
        }
    }

    fn internal_query<'a, T>(
        &self,
        index: &'a BTreeSet<EventIndex>,
        deleted_ids: &'a HashSet<EventId>,
        filter: T,
    ) -> impl Iterator<Item = &'a EventIndex>
    where
        T: Into<FilterIndex>,
    {
        self.internal_parallel_query(index, deleted_ids, filter)
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn internal_parallel_query<'a, T>(
        &self,
        index: &'a BTreeSet<EventIndex>,
        deleted_ids: &'a HashSet<EventId>,
        filter: T,
    ) -> impl ParallelIterator<Item = &'a EventIndex>
    where
        T: Into<FilterIndex>,
    {
        let filter: FilterIndex = filter.into();
        index.par_iter().filter(move |event| {
            !deleted_ids.contains(&event.event_id) && filter.match_event(event)
        })
    }

    /// Query
    #[tracing::instrument(skip_all, level = "trace")]
    pub async fn query<I>(&self, filters: I) -> Vec<EventId>
    where
        I: IntoIterator<Item = Filter>,
    {
        let index = self.index.read().await;
        let deleted_ids = self.deleted_ids.read().await;

        let mut matching_ids: BTreeSet<&EventIndex> = BTreeSet::new();

        for filter in filters.into_iter() {
            if filter.is_empty() {
                return index.iter().map(|e| e.event_id).collect();
            }

            if let (Some(since), Some(until)) = (filter.since, filter.until) {
                if since > until {
                    continue;
                }
            }

            if let Some(limit) = filter.limit {
                matching_ids.par_extend(
                    self.internal_query(&index, &deleted_ids, filter)
                        .take(limit)
                        .par_bridge(),
                )
            } else {
                matching_ids.par_extend(self.internal_parallel_query(&index, &deleted_ids, filter))
            }
        }

        matching_ids.into_iter().map(|e| e.event_id).collect()
    }

    /// Count events
    #[tracing::instrument(skip_all, level = "trace")]
    pub async fn count<I>(&self, filters: I) -> usize
    where
        I: IntoIterator<Item = Filter>,
    {
        let index = self.index.read().await;
        let deleted_ids = self.deleted_ids.read().await;

        let mut counter: usize = 0;

        for filter in filters.into_iter() {
            if filter.is_empty() {
                counter = index.len();
                break;
            }

            if let (Some(since), Some(until)) = (filter.since, filter.until) {
                if since > until {
                    continue;
                }
            }

            let limit: Option<usize> = filter.limit;
            let count = self
                .internal_parallel_query(&index, &deleted_ids, filter)
                .count();
            if let Some(limit) = limit {
                let count = if limit >= count { limit } else { count };
                counter += count;
            } else {
                counter += count;
            }
        }

        counter
    }

    /// Check if an event with [`EventId`] has been deleted
    pub async fn has_event_id_been_deleted(&self, event_id: &EventId) -> bool {
        let deleted_ids = self.deleted_ids.read().await;
        deleted_ids.contains(event_id)
    }

    /// Check if event with [`Coordinate`] has been deleted before [`Timestamp`]
    pub async fn has_coordinate_been_deleted(
        &self,
        coordinate: &Coordinate,
        timestamp: Timestamp,
    ) -> bool {
        let deleted_coordinates = self.deleted_coordinates.read().await;
        if let Some(t) = deleted_coordinates.get(coordinate).copied() {
            t >= timestamp
        } else {
            false
        }
    }

    /// Clear indexes
    pub async fn clear(&self) {
        let mut index = self.index.write().await;
        let mut deleted_ids = self.deleted_ids.write().await;
        let mut deleted_coordinates = self.deleted_coordinates.write().await;
        index.clear();
        deleted_ids.clear();
        deleted_coordinates.clear();
    }
}

#[cfg(test)]
mod tests {
    use nostr::nips::nip01::Coordinate;
    use nostr::secp256k1::SecretKey;
    use nostr::{EventBuilder, FromBech32, Keys, Tag};

    use super::*;

    const SECRET_KEY_A: &str = "nsec1ufnus6pju578ste3v90xd5m2decpuzpql2295m3sknqcjzyys9ls0qlc85";
    const SECRET_KEY_B: &str = "nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99";

    #[tokio::test]
    async fn test_database_indexes() {
        let indexes = DatabaseIndexes::new();

        // Keys
        let keys_a = Keys::new(SecretKey::from_bech32(SECRET_KEY_A).unwrap());
        let keys_b = Keys::new(SecretKey::from_bech32(SECRET_KEY_B).unwrap());

        // Build some events
        let events = [
            EventBuilder::new_text_note("Text note", [])
                .to_event(&keys_a)
                .unwrap(),
            EventBuilder::new(
                Kind::ParameterizedReplaceable(32121),
                "Empty 1",
                [Tag::Identifier(String::from("abdefgh:12345678"))],
            )
            .to_event(&keys_a)
            .unwrap(),
            EventBuilder::new(
                Kind::ParameterizedReplaceable(32122),
                "Empty 2",
                [Tag::Identifier(String::from("abdefgh:12345678"))],
            )
            .to_event(&keys_a)
            .unwrap(),
            EventBuilder::new(
                Kind::ParameterizedReplaceable(32122),
                "",
                [Tag::Identifier(String::from("ijklmnop:87654321"))],
            )
            .to_event(&keys_a)
            .unwrap(),
            EventBuilder::new(
                Kind::ParameterizedReplaceable(32122),
                "",
                [Tag::Identifier(String::from("abdefgh:87654321"))],
            )
            .to_event(&keys_b)
            .unwrap(),
            EventBuilder::new(
                Kind::ParameterizedReplaceable(32122),
                "",
                [Tag::Identifier(String::from("abdefgh:12345678"))],
            )
            .to_event(&keys_b)
            .unwrap(),
            EventBuilder::new(
                Kind::ParameterizedReplaceable(32122),
                "Test",
                [Tag::Identifier(String::from("abdefgh:12345678"))],
            )
            .to_event(&keys_a)
            .unwrap(),
        ];

        for event in events.iter() {
            indexes.index_event(event).await;
        }

        // Total events: 6

        let filter = Filter::new();
        assert_eq!(indexes.count([filter]).await, 6);

        // Invalid event deletion (wrong signing keys)
        let coordinate =
            Coordinate::new(Kind::ParameterizedReplaceable(32122), keys_a.public_key());
        let event = EventBuilder::delete([coordinate])
            .to_event(&keys_b)
            .unwrap();
        indexes.index_event(&event).await;

        // Total events: 7 (6 + 1)

        let filter = Filter::new();
        assert_eq!(indexes.count([filter]).await, 7);

        // Delete valid event
        let coordinate =
            Coordinate::new(Kind::ParameterizedReplaceable(32122), keys_a.public_key())
                .identifier("ijklmnop:87654321");
        let event = EventBuilder::delete([coordinate])
            .to_event(&keys_a)
            .unwrap();
        indexes.index_event(&event).await;

        // Total events: 7 (7 - 1 + 1)

        // Check total number of indexes
        let filter = Filter::new();
        assert_eq!(indexes.count([filter]).await, 7);

        // Check if query len and count match
        assert_eq!(
            indexes.query([Filter::new()]).await.len(),
            indexes.count([Filter::new()]).await
        );
    }
}
