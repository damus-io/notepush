use crate::{notification_manager::nostr_event_extensions::MaybeConvertibleToTimestampedMuteList, utils::time_delta::TimeDelta};
use tokio::time::Duration;
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use log;
use super::nostr_event_extensions::{MaybeConvertibleToRelayList, RelayList, TimestampedMuteList};

struct CacheEntry<T> {
    value: Option<T>,   // `None` means the event does not exist as far as we know (It does NOT mean expired)
    added_at: nostr::Timestamp,
}

impl<T> CacheEntry<T> {
    fn is_expired(&self, max_age: Duration) -> bool {
        let time_delta = TimeDelta::subtracting(nostr::Timestamp::now(), self.added_at);
        time_delta.negative || (time_delta.delta_abs_seconds > max_age.as_secs())
    }
}

impl<T> CacheEntry<T> {
    pub fn new(value: T) -> Self {
        let added_at = nostr::Timestamp::now();
        CacheEntry { value: Some(value), added_at }
    }

    pub fn maybe(value: Option<T>) -> Self {
        let added_at = nostr::Timestamp::now();
        CacheEntry { value, added_at }
    }

    pub fn empty() -> Self {
        let added_at = nostr::Timestamp::now();
        CacheEntry { value: None, added_at }
    }

    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }
}

pub struct Cache {
    //entries: HashMap<EventId, Event>,
    mute_lists: HashMap<PublicKey, CacheEntry<TimestampedMuteList>>,
    contact_lists: HashMap<PublicKey, CacheEntry<Event>>,
    relay_lists: HashMap<PublicKey, CacheEntry<RelayList>>,
    max_age: Duration,
}

fn get_cache_entry<T: Clone>(list: &mut HashMap<PublicKey, CacheEntry<T>>, pubkey: &PublicKey, max_age: Duration, name: &str) -> Result<Option<T>, CacheError> {
    let res = if let Some(entry) = list.get(pubkey) {
        if !entry.is_expired(max_age) {
            Ok(entry.value().cloned())
        } else {
            log::debug!("{} list for pubkey {} is expired, removing it from the cache", name, pubkey.to_hex());
            Err(CacheError::Expired)
        }
    } else {
        Err(CacheError::NotFound)
    };

    if let Err(CacheError::Expired) = &res {
        list.remove(pubkey);
    }

    res
}

impl Cache {
    // MARK: - Initialization

    pub fn new(max_age: Duration) -> Self {
        Cache {
            //entries: HashMap::new(),
            mute_lists: HashMap::new(),
            contact_lists: HashMap::new(),
            relay_lists: HashMap::new(),
            max_age,
        }
    }

    // MARK: - Adding items to the cache

    pub fn add_optional_mute_list_with_author<'a>(&'a mut self, author: &PublicKey, mute_list: Option<&Event>) {
        if let Some(mute_list) = mute_list {
            self.add_event(mute_list);
        } else {
            self.mute_lists.insert(
                author.to_owned(),
                CacheEntry::empty(),
            );
        }
    }

    pub fn add_optional_relay_list_with_author<'a>(&'a mut self, author: &PublicKey, relay_list_event: Option<&Event>) {
        if let Some(relay_list_event) = relay_list_event {
            self.add_event(relay_list_event);
        } else {
            self.relay_lists.insert(
                author.to_owned(),
                CacheEntry::empty(),
            );
        }
    }

    pub fn add_optional_contact_list_with_author<'a>(&'a mut self, author: &PublicKey, contact_list: Option<&Event>) {
        if let Some(contact_list) = contact_list {
            self.add_event(contact_list);
        } else {
            self.contact_lists.insert(
                author.to_owned(),
                CacheEntry::empty(),
            );
        }
    }

    pub fn add_event(&mut self, event: &Event) {
        match event.kind {
            Kind::MuteList => {
                self.mute_lists.insert(event.pubkey.clone(), CacheEntry::maybe(event.to_timestamped_mute_list()));
                log::debug!("Added mute list to the cache. Event ID: {}", event.id.to_hex());
            }
            Kind::ContactList => {
                log::debug!("Added contact list to the cache. Event ID: {}", event.id.to_hex());
                self.contact_lists.insert(event.pubkey.clone(), CacheEntry::new(event.to_owned()));
            },
            Kind::RelayList => {
                log::debug!("Added relay list to the cache. Event ID: {}", event.id.to_hex());
                self.relay_lists.insert(event.pubkey.clone(), CacheEntry::maybe(event.to_relay_list()));
            },
            _ => {
                log::debug!("Unknown event kind, not adding to any cache. Event ID: {}", event.id.to_hex());
            }
        }
    }

    // MARK: - Fetching items from the cache

    pub fn get_mute_list(&mut self, pubkey: &PublicKey) -> Result<Option<TimestampedMuteList>, CacheError> {
        get_cache_entry(&mut self.mute_lists, pubkey, self.max_age, "Mute")
    }

    pub fn get_relay_list(&mut self, pubkey: &PublicKey) -> Result<Option<RelayList>, CacheError> {
        get_cache_entry(&mut self.relay_lists, pubkey, self.max_age, "Relay")
    }

    pub fn get_contact_list(&mut self, pubkey: &PublicKey) -> Result<Option<Event>, CacheError> {
        get_cache_entry(&mut self.contact_lists, pubkey, self.max_age, "Contact")
    }
}

// Error type
#[derive(Debug)]
pub enum CacheError {
    NotFound,
    Expired,
}
