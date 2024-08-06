use crate::utils::time_delta::TimeDelta;
use tokio::time::Duration;
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use log;

use super::nostr_event_extensions::MaybeConvertibleToMuteList;

struct CacheEntry {
    event: Option<Event>,   // `None` means the event does not exist as far as we know (It does NOT mean expired)
    added_at: nostr::Timestamp,
}

impl CacheEntry {
    fn is_expired(&self, max_age: Duration) -> bool {
        let time_delta = TimeDelta::subtracting(nostr::Timestamp::now(), self.added_at);
        time_delta.negative || (time_delta.delta_abs_seconds > max_age.as_secs())
    }
}

pub struct Cache {
    entries: HashMap<EventId, Arc<CacheEntry>>,
    mute_lists: HashMap<PublicKey, Arc<CacheEntry>>,
    contact_lists: HashMap<PublicKey, Arc<CacheEntry>>,
    max_age: Duration,
}

impl Cache {
    // MARK: - Initialization

    pub fn new(max_age: Duration) -> Self {
        Cache {
            entries: HashMap::new(),
            mute_lists: HashMap::new(),
            contact_lists: HashMap::new(),
            max_age,
        }
    }

    // MARK: - Adding items to the cache
    
    pub fn add_optional_mute_list_with_author(&mut self, author: &PublicKey, mute_list: Option<Event>) {
        if let Some(mute_list) = mute_list {
            self.add_event(mute_list);
        } else {
            self.mute_lists.insert(
                author.clone(),
                Arc::new(CacheEntry {
                    event: None,
                    added_at: nostr::Timestamp::now(),
                }),
            );
        }
    }
    
    pub fn add_optional_contact_list_with_author(&mut self, author: &PublicKey, contact_list: Option<Event>) {
        if let Some(contact_list) = contact_list {
            self.add_event(contact_list);
        } else {
            self.contact_lists.insert(
                author.clone(),
                Arc::new(CacheEntry {
                    event: None,
                    added_at: nostr::Timestamp::now(),
                }),
            );
        }
    }

    pub fn add_event(&mut self, event: Event) {
        let entry = Arc::new(CacheEntry {
            event: Some(event.clone()),
            added_at: nostr::Timestamp::now(),
        });
        self.entries.insert(event.id.clone(), entry.clone());

        match event.kind {
            Kind::MuteList => {
                self.mute_lists.insert(event.pubkey.clone(), entry.clone());
                log::debug!("Added mute list to the cache. Event ID: {}", event.id.to_hex());
            }
            Kind::ContactList => {
                self.contact_lists
                    .insert(event.pubkey.clone(), entry.clone());
                log::debug!("Added contact list to the cache. Event ID: {}", event.id.to_hex());
            }
            _ => {
                log::debug!("Added event to the cache. Event ID: {}", event.id.to_hex());
            }
        }
    }

    // MARK: - Fetching items from the cache

    pub fn get_mute_list(&mut self, pubkey: &PublicKey) -> Result<Option<MuteList>, CacheError> {
        if let Some(entry) = self.mute_lists.get(pubkey) {
            let entry = entry.clone();  // Clone the Arc to avoid borrowing issues
            if !entry.is_expired(self.max_age) {
                if let Some(event) = entry.event.clone() {
                    return Ok(event.to_mute_list());
                }
            } else {
                log::debug!("Mute list for pubkey {} is expired, removing it from the cache", pubkey.to_hex());
                self.mute_lists.remove(pubkey);
                self.remove_event_from_all_maps(&entry.event);
            }
        }
        Err(CacheError::NotFound)
    }

    pub fn get_contact_list(&mut self, pubkey: &PublicKey) -> Result<Option<Event>, CacheError> {
        if let Some(entry) = self.contact_lists.get(pubkey) {
            let entry = entry.clone();  // Clone the Arc to avoid borrowing issues
            if !entry.is_expired(self.max_age) {
                return Ok(entry.event.clone());
            } else {
                log::debug!("Contact list for pubkey {} is expired, removing it from the cache", pubkey.to_hex());
                self.contact_lists.remove(pubkey);
                self.remove_event_from_all_maps(&entry.event);
            }
        }
        Err(CacheError::NotFound)
    }

    // MARK: - Removing items from the cache

    fn remove_event_from_all_maps(&mut self, event: &Option<Event>) {
        if let Some(event) = event {
            let event_id = event.id.clone();
            let pubkey = event.pubkey.clone();
            self.entries.remove(&event_id);
            self.mute_lists.remove(&pubkey);
            self.contact_lists.remove(&pubkey);
        }
        // We can't remove an event from all maps if the event does not exist
    }
}

// Error type
#[derive(Debug)]
pub enum CacheError {
    NotFound,
}
