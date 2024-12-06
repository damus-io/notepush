use tokio::sync::Mutex;
use super::nostr_event_extensions::{RelayList, TimestampedMuteList};
use super::notification_manager::EventSaver;
use super::ExtendedEvent;
use nostr_sdk::prelude::*;
use super::nostr_event_cache::Cache;
use tokio::time::{timeout, Duration};

const NOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

pub struct NostrNetworkHelper {
    bootstrap_client: Client,
    cache: Mutex<Cache>,
    event_saver: EventSaver
}

impl NostrNetworkHelper {
    // MARK: - Initialization

    pub async fn new(relay_url: String, cache_max_age: Duration, event_saver: EventSaver) -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::new(&Keys::generate());
        client.add_relay(relay_url.clone()).await?;
        client.connect().await;
        Ok(NostrNetworkHelper {
            bootstrap_client: client,
            cache: Mutex::new(Cache::new(cache_max_age)),
            event_saver,
        })
    }

    // MARK: - Answering questions about a user

    pub async fn does_pubkey_follow_pubkey(
        &self,
        source_pubkey: &PublicKey,
        target_pubkey: &PublicKey,
    ) -> bool {
        log::debug!(
            "Checking if pubkey {:?} follows pubkey {:?}",
            source_pubkey,
            target_pubkey
        );
        if let Some(contact_list) = self.get_contact_list(source_pubkey).await {
            return contact_list.referenced_pubkeys().contains(target_pubkey);
        }
        false
    }

    // MARK: - Getting specific event types with caching

    pub async fn get_public_mute_list(&self, pubkey: &PublicKey) -> Option<TimestampedMuteList> {
        {
            let mut cache_mutex_guard = self.cache.lock().await;
            if let Ok(optional_mute_list) = cache_mutex_guard.get_mute_list(pubkey) {
                return optional_mute_list;
            }
        }   // Release the lock here for improved performance

        // We don't have an answer from the cache, so we need to fetch it
        let mute_list_event = self.fetch_single_event(pubkey, Kind::MuteList).await;
        let mut cache_mutex_guard = self.cache.lock().await;
        cache_mutex_guard.add_optional_mute_list_with_author(pubkey, mute_list_event.as_ref());
        cache_mutex_guard.get_mute_list(pubkey).ok()?
    }

    pub async fn get_relay_list(&self, pubkey: &PublicKey) -> Option<RelayList> {
        {
            let mut cache_mutex_guard = self.cache.lock().await;
            if let Ok(optional_relay_list) = cache_mutex_guard.get_relay_list(pubkey) {
                return optional_relay_list;
            }
        }   // Release the lock here for improved performance

        // We don't have an answer from the cache, so we need to fetch it
        let relay_list_event = NostrNetworkHelper::fetch_single_event_from_client(pubkey, Kind::RelayList, &self.bootstrap_client).await;
        let mut cache_mutex_guard = self.cache.lock().await;
        cache_mutex_guard.add_optional_relay_list_with_author(pubkey, relay_list_event.as_ref());
        cache_mutex_guard.get_relay_list(pubkey).ok()?
    }

    pub async fn get_contact_list(&self, pubkey: &PublicKey) -> Option<Event> {
        {
            let mut cache_mutex_guard = self.cache.lock().await;
            if let Ok(optional_contact_list) = cache_mutex_guard.get_contact_list(pubkey) {
                return optional_contact_list;
            }
        }   // Release the lock here for improved performance

        // We don't have an answer from the cache, so we need to fetch it
        let contact_list_event = self.fetch_single_event(pubkey, Kind::ContactList).await;
        let mut cache_mutex_guard = self.cache.lock().await;
        cache_mutex_guard.add_optional_contact_list_with_author(pubkey, contact_list_event.as_ref());
        cache_mutex_guard.get_contact_list(pubkey).ok()?
    }

    // MARK: - Lower level fetching functions

    async fn fetch_single_event(&self, author: &PublicKey, kind: Kind) -> Option<Event> {
        let event = match self.make_client_for(author).await {
            Some(client) => {
                NostrNetworkHelper::fetch_single_event_from_client(author, kind, &client).await
            },
            None => {
                NostrNetworkHelper::fetch_single_event_from_client(author, kind, &self.bootstrap_client).await
            },
        };
        // Save event to our database if needed
        if let Some(event) = event.clone() {
            if let Err(error) = self.event_saver.save_if_needed(&event).await {
                log::warn!("Failed to save event '{:?}'. Error: {:?}", event.id.to_hex(), error)
            }
        }
        event
    }

    async fn make_client_for(&self, author: &PublicKey) -> Option<Client> {
        let client = Client::new(&Keys::generate());

        let relay_list = self.get_relay_list(author).await?;
        for (url, metadata) in relay_list {
            if metadata.map_or(true, |m| m == RelayMetadata::Write) {   // Only add "write" relays, as per NIP-65 spec on reading data FROM user
                if let Err(e) = client.add_relay(url.clone()).await {
                    log::warn!("Failed to add relay URL: {:?}, error: {:?}", url, e);
                }
            }
        }

        client.connect().await;

        Some(client)
    }

    async fn fetch_single_event_from_client(author: &PublicKey, kind: Kind, client: &Client) -> Option<Event> {
        let subscription_filter = Filter::new()
            .kinds(vec![kind])
            .authors(vec![author.clone()])
            .limit(1);

        let mut notifications = client.notifications();
        let this_subscription_id = client
            .subscribe(Vec::from([subscription_filter]), None)
            .await;

        let mut event: Option<Event> = None;

        while let Ok(result) = timeout(NOTE_FETCH_TIMEOUT, notifications.recv()).await {
            if let Ok(notification) = result {
                if let RelayPoolNotification::Event {
                    subscription_id,
                    event: event_option,
                    ..
                } = notification
                {
                    if this_subscription_id == subscription_id && event_option.kind == kind {
                        event = Some((*event_option).clone());
                        break;
                    }
                }
            }
        }

        if event.is_none() {
            log::info!("Event of kind {:?} not found for pubkey {:?}", kind, author);
        }

        client.unsubscribe(this_subscription_id).await;
        event
    }
}
