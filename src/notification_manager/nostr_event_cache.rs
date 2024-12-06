use crate::{notification_manager::nostr_event_extensions::MaybeConvertibleToTimestampedMuteList};
use tokio::time::{Duration, Instant};
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use log;
use super::nostr_event_extensions::{MaybeConvertibleToRelayList, RelayList, TimestampedMuteList};

struct CacheEntry<T> {
    value: Option<T>,   // `None` means the event does not exist as far as we know (It does NOT mean expired)
    added_at: Instant,
}

impl<T> CacheEntry<T> {
    fn is_expired(&self, max_age: Duration) -> bool {
        self.added_at.elapsed() > max_age
    }

    pub fn new(value: T) -> Self {
        let added_at = Instant::now();
        CacheEntry { value: Some(value), added_at }
    }

    pub fn maybe(value: Option<T>) -> Self {
        let added_at = Instant::now();
        CacheEntry { value, added_at }
    }

    pub fn empty() -> Self {
        let added_at = Instant::now();
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
#[derive(Debug, Eq, PartialEq)]
pub enum CacheError {
    NotFound,
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::*;
    use std::time::Duration;
    use tokio::time::sleep;
    use std::str::FromStr;

    // Helper function to create a dummy event of a given kind for testing.
    fn create_dummy_event(pubkey: PublicKey, kind: Kind) -> Event {
        // In a real test, you might generate keys or events more dynamically.
        let id = EventId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let created_at = Timestamp::now();
        let content = "";
        let sig_str = "8e1a61523765a6e577e3ca0c87afe3694ed518719aea067701c35262dd2a3c7e3ca0946fe98463a3af706dd333695ceec6cb3b29254c557c8630d3db1171ea3d";
        let sig = Signature::from_str(sig_str).unwrap();

        Event::new(id, pubkey, created_at, kind, [], content, sig)
    }

    // Helper function to create a dummy public key for testing.
    fn create_dummy_pubkey() -> PublicKey {
        // In a real project, you'd generate a key. For the sake of tests, just parse a known hex.
        PublicKey::from_hex("32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245").unwrap()
    }

    #[tokio::test]
    async fn test_add_and_retrieve_contact_list() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age);

        // Initially, no contact list should be found.
        assert!(matches!(cache.get_contact_list(&pubkey), Err(CacheError::NotFound)));

        // Add a contact list event.
        let event = create_dummy_event(pubkey, Kind::ContactList);
        cache.add_event(&event);

        // Now we should be able to retrieve it.
        let retrieved = cache.get_contact_list(&pubkey).unwrap();
        assert!(retrieved.is_some());
        let retrieved_event = retrieved.unwrap();
        assert_eq!(retrieved_event.id, event.id);
    }

    #[tokio::test]
    async fn test_add_and_retrieve_mute_list() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age);

        // No mute list initially
        assert!(matches!(cache.get_mute_list(&pubkey), Err(CacheError::NotFound)));

        // Add a mute list event.
        let mutelist_event = {
            let event = create_dummy_event(pubkey, Kind::MuteList);
            event
        };

        cache.add_event(&mutelist_event);
        let retrieved = cache.get_mute_list(&pubkey).unwrap();
        assert!(retrieved.is_some()); // Should have a Some(TimestampedMuteList) now
    }

    #[tokio::test]
    async fn test_add_and_retrieve_relay_list() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age);

        // No relay list initially
        assert!(matches!(cache.get_relay_list(&pubkey), Err(CacheError::NotFound)));

        // Add a relay list event.
        let relaylist_event = create_dummy_event(pubkey, Kind::RelayList);
        cache.add_event(&relaylist_event);

        let retrieved = cache.get_relay_list(&pubkey).unwrap();
        assert!(retrieved.is_some());
    }

    #[tokio::test]
    async fn test_expired_entries() {
        // Very short max_age to test expiration logic quickly.
        let max_age = Duration::from_millis(100);
        let pubkey = create_dummy_pubkey();
        let mut cache = Cache::new(max_age);

        // Add a contact list event that will expire soon.
        let event = create_dummy_event(pubkey, Kind::ContactList);
        cache.add_event(&event);

        // Initially, we can retrieve it.
        let retrieved = cache.get_contact_list(&pubkey).unwrap();
        assert!(retrieved.is_some());

        // Wait for it to expire.
        sleep(Duration::from_millis(200)).await;

        // Now it should be expired and removed.
        let result = cache.get_contact_list(&pubkey);
        assert_eq!(result, Err(CacheError::Expired));
    }

    #[tokio::test]
    async fn test_empty_entries() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age);

        // Add empty mute list
        cache.add_optional_mute_list_with_author(&pubkey, None);

        // We should now find a mute list entry, but it's None.
        let result = cache.get_mute_list(&pubkey).unwrap();
        assert!(result.is_none());

        // Add empty contact list
        cache.add_optional_contact_list_with_author(&pubkey, None);

        let result = cache.get_contact_list(&pubkey).unwrap();
        assert!(result.is_none());

        // Add empty relay list
        cache.add_optional_relay_list_with_author(&pubkey, None);

        let result = cache.get_relay_list(&pubkey).unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_re_insertion() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age);

        // Insert empty first
        cache.add_optional_contact_list_with_author(&pubkey, None);
        assert!(cache.get_contact_list(&pubkey).unwrap().is_none());

        // Now insert a real event
        let event = create_dummy_event(pubkey, Kind::ContactList);
        cache.add_event(&event);

        // It should now return the actual event
        let retrieved = cache.get_contact_list(&pubkey).unwrap().unwrap();
        assert_eq!(retrieved.id, event.id);
    }
}
