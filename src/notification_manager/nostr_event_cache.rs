use super::nostr_event_extensions::{MaybeConvertibleToRelayList, RelayList, TimestampedMuteList};
use crate::notification_manager::nostr_event_extensions::MaybeConvertibleToTimestampedMuteList;
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use tokio::time::{Duration, Instant};

struct CacheEntry<T> {
    value: Option<T>, // `None` means the event does not exist as far as we know (It does NOT mean expired)
    added_at: Instant,
}

impl<T> CacheEntry<T> {
    fn is_expired(&self, max_age: Duration) -> bool {
        self.added_at.elapsed() > max_age
    }

    pub fn new(value: T) -> Self {
        let added_at = Instant::now();
        CacheEntry {
            value: Some(value),
            added_at,
        }
    }

    pub fn maybe(value: Option<T>) -> Self {
        let added_at = Instant::now();
        CacheEntry { value, added_at }
    }

    pub fn empty() -> Self {
        let added_at = Instant::now();
        CacheEntry {
            value: None,
            added_at,
        }
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
    max_entries: usize,
}

fn get_cache_entry<T: Clone>(
    list: &mut HashMap<PublicKey, CacheEntry<T>>,
    pubkey: &PublicKey,
    max_age: Duration,
    name: &str,
) -> Result<Option<T>, CacheError> {
    let res = if let Some(entry) = list.get(pubkey) {
        if !entry.is_expired(max_age) {
            Ok(entry.value().cloned())
        } else {
            log::debug!(
                "{} list for pubkey {} is expired, removing it from the cache",
                name,
                pubkey.to_hex()
            );
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

fn remove_expired_from<T>(
    max_age: Duration,
    name: &str,
    map: &mut HashMap<PublicKey, CacheEntry<T>>,
) {
    let expired_keys: Vec<PublicKey> = map
        .iter()
        .filter(|(_, entry)| entry.is_expired(max_age))
        .map(|(pubkey, _)| pubkey.to_owned())
        .collect();

    if expired_keys.is_empty() {
        return;
    }

    for pubkey in expired_keys {
        log::debug!(
            "{} list for pubkey {} expired, pruning from cache",
            name,
            pubkey.to_hex()
        );
        map.remove(&pubkey);
    }
}

fn enforce_cap<T>(
    max_entries: usize,
    name: &str,
    map: &mut HashMap<PublicKey, CacheEntry<T>>,
) {
    if map.len() <= max_entries {
        return;
    }

    // Remove oldest entries first based on insertion time to respect cap.
    let mut entries: Vec<(PublicKey, Instant)> = map
        .iter()
        .map(|(pubkey, entry)| (pubkey.to_owned(), entry.added_at))
        .collect();

    entries.sort_by_key(|(_, added_at)| *added_at);

    let remove_count = map.len() - max_entries;
    for (pubkey, _) in entries.into_iter().take(remove_count) {
        log::debug!(
            "{} cache over cap ({}). Evicting pubkey {}.",
            name,
            max_entries,
            pubkey.to_hex()
        );
        map.remove(&pubkey);
    }
}

impl Cache {
    // MARK: - Initialization

    pub fn new(max_age: Duration, max_entries: usize) -> Self {
        Cache {
            //entries: HashMap::new(),
            mute_lists: HashMap::new(),
            contact_lists: HashMap::new(),
            relay_lists: HashMap::new(),
            max_age,
            max_entries,
        }
    }

    /// Remove expired entries across all caches so we don't retain stale keys forever.
    /// This runs eagerly before add/get operations rather than waiting for callers to
    /// request a specific key (which could leave expired keys resident indefinitely).
    pub fn prune_expired_entries(&mut self) {
        let max_age = self.max_age;
        remove_expired_from(max_age, "Mute", &mut self.mute_lists);
        remove_expired_from(max_age, "Contact", &mut self.contact_lists);
        remove_expired_from(max_age, "Relay", &mut self.relay_lists);
    }

    // MARK: - Adding items to the cache

    pub fn add_optional_mute_list_with_author(
        &mut self,
        author: &PublicKey,
        mute_list: Option<&Event>,
    ) {
        self.prune_expired_entries();
        if let Some(mute_list) = mute_list {
            self.add_event(mute_list);
        } else {
            self.mute_lists
                .insert(author.to_owned(), CacheEntry::empty());
        }
        enforce_cap(self.max_entries, "Mute", &mut self.mute_lists);
    }

    pub fn add_optional_relay_list_with_author(
        &mut self,
        author: &PublicKey,
        relay_list_event: Option<&Event>,
    ) {
        self.prune_expired_entries();
        if let Some(relay_list_event) = relay_list_event {
            self.add_event(relay_list_event);
        } else {
            self.relay_lists
                .insert(author.to_owned(), CacheEntry::empty());
        }
        enforce_cap(self.max_entries, "Relay", &mut self.relay_lists);
    }

    pub fn add_optional_contact_list_with_author(
        &mut self,
        author: &PublicKey,
        contact_list: Option<&Event>,
    ) {
        self.prune_expired_entries();
        if let Some(contact_list) = contact_list {
            self.add_event(contact_list);
        } else {
            self.contact_lists
                .insert(author.to_owned(), CacheEntry::empty());
        }
        enforce_cap(self.max_entries, "Contact", &mut self.contact_lists);
    }

    pub fn add_event(&mut self, event: &Event) {
        self.prune_expired_entries();
        match event.kind {
            Kind::MuteList => {
                self.mute_lists.insert(
                    event.pubkey,
                    CacheEntry::maybe(event.to_timestamped_mute_list()),
                );
                log::debug!(
                    "Added mute list to the cache. Event ID: {}",
                    event.id.to_hex()
                );
            }
            Kind::ContactList => {
                log::debug!(
                    "Added contact list to the cache. Event ID: {}",
                    event.id.to_hex()
                );
                self.contact_lists
                    .insert(event.pubkey, CacheEntry::new(event.to_owned()));
            }
            Kind::RelayList => {
                log::debug!(
                    "Added relay list to the cache. Event ID: {}",
                    event.id.to_hex()
                );
                self.relay_lists
                    .insert(event.pubkey, CacheEntry::maybe(event.to_relay_list()));
            }
            _ => {
                log::debug!(
                    "Unknown event kind, not adding to any cache. Event ID: {}",
                    event.id.to_hex()
                );
            }
        }

        enforce_cap(self.max_entries, "Mute", &mut self.mute_lists);
        enforce_cap(self.max_entries, "Contact", &mut self.contact_lists);
        enforce_cap(self.max_entries, "Relay", &mut self.relay_lists);
    }

    // MARK: - Fetching items from the cache

    pub fn get_mute_list(
        &mut self,
        pubkey: &PublicKey,
    ) -> Result<Option<TimestampedMuteList>, CacheError> {
        self.prune_expired_entries();
        get_cache_entry(&mut self.mute_lists, pubkey, self.max_age, "Mute")
    }

    pub fn get_relay_list(&mut self, pubkey: &PublicKey) -> Result<Option<RelayList>, CacheError> {
        self.prune_expired_entries();
        get_cache_entry(&mut self.relay_lists, pubkey, self.max_age, "Relay")
    }

    pub fn get_contact_list(&mut self, pubkey: &PublicKey) -> Result<Option<Event>, CacheError> {
        self.prune_expired_entries();
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
    use std::str::FromStr;
    use std::time::Duration;
    use tokio::time::sleep;

    // Helper function to create a dummy event of a given kind for testing.
    fn create_dummy_event(pubkey: PublicKey, kind: Kind) -> Event {
        // In a real test, you might generate keys or events more dynamically.
        let id =
            EventId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        let created_at = Timestamp::now();
        let content = "";
        let sig_str = "8e1a61523765a6e577e3ca0c87afe3694ed518719aea067701c35262dd2a3c7e3ca0946fe98463a3af706dd333695ceec6cb3b29254c557c8630d3db1171ea3d";
        let sig = Signature::from_str(sig_str).unwrap();

        Event::new(id, pubkey, created_at, kind, [], content, sig)
    }

    // Helper function to create a dummy public key for testing.
    fn create_dummy_pubkey() -> PublicKey {
        // In a real project, you'd generate a key. For the sake of tests, just parse a known hex.
        PublicKey::from_hex("32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245")
            .unwrap()
    }

    fn create_unique_pubkey() -> PublicKey {
        // Use random key generation to get distinct, valid pubkeys for eviction tests.
        Keys::generate().public_key()
    }

    #[tokio::test]
    async fn test_add_and_retrieve_contact_list() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age, 10);

        // Initially, no contact list should be found.
        assert!(matches!(
            cache.get_contact_list(&pubkey),
            Err(CacheError::NotFound)
        ));

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
        let mut cache = Cache::new(max_age, 10);

        // No mute list initially
        assert!(matches!(
            cache.get_mute_list(&pubkey),
            Err(CacheError::NotFound)
        ));

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
        let mut cache = Cache::new(max_age, 10);

        // No relay list initially
        assert!(matches!(
            cache.get_relay_list(&pubkey),
            Err(CacheError::NotFound)
        ));

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
        let mut cache = Cache::new(max_age, 10);

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
        // Pruning runs on get, so expired entries are removed before lookup.
        assert_eq!(result, Err(CacheError::NotFound));
    }

    #[tokio::test]
    async fn test_prune_removes_expired_entries_without_access() {
        // Guard against stale cache growth by pruning after the TTL passes.
        let max_age = Duration::from_millis(50);
        let pubkey = create_dummy_pubkey();
        let mut cache = Cache::new(max_age, 10);

        let contact_event = create_dummy_event(pubkey, Kind::ContactList);
        cache.add_event(&contact_event);

        // Add an empty entry to ensure we also clear cached misses.
        cache.add_optional_relay_list_with_author(&pubkey, None);

        sleep(Duration::from_millis(75)).await;

        cache.prune_expired_entries();

        assert!(matches!(
            cache.get_contact_list(&pubkey),
            Err(CacheError::NotFound)
        ));
        assert!(matches!(cache.get_relay_list(&pubkey), Err(CacheError::NotFound)));
    }

    #[tokio::test]
    async fn test_empty_entries() {
        let pubkey = create_dummy_pubkey();
        let max_age = Duration::from_secs(60);
        let mut cache = Cache::new(max_age, 10);

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
        let mut cache = Cache::new(max_age, 10);

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

    #[tokio::test]
    async fn test_cap_evicts_oldest_entries() {
        let max_age = Duration::from_secs(60);
        let max_entries = 2;
        let mut cache = Cache::new(max_age, max_entries);

        let pubkey_a = create_unique_pubkey();
        let pubkey_b = create_unique_pubkey();
        let pubkey_c = create_unique_pubkey();

        cache.add_event(&create_dummy_event(pubkey_a, Kind::ContactList));
        cache.add_event(&create_dummy_event(pubkey_b, Kind::ContactList));

        // Third insert should evict the oldest (pubkey_a) to honor cap.
        cache.add_event(&create_dummy_event(pubkey_c, Kind::ContactList));

        assert!(matches!(
            cache.get_contact_list(&pubkey_a),
            Err(CacheError::NotFound)
        ));

        let b_contact = cache.get_contact_list(&pubkey_b).unwrap();
        let c_contact = cache.get_contact_list(&pubkey_c).unwrap();
        assert!(b_contact.is_some());
        assert!(c_contact.is_some());
    }
}
