use super::nostr_event_extensions::MaybeConvertibleToMuteList;
use super::ExtendedEvent;
use nostr_sdk::prelude::*;
use super::nostr_event_cache::Cache;
use tokio::time::{timeout, Duration};

const NOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_MAX_AGE: Duration = Duration::from_secs(60);

pub struct NostrNetworkHelper {
    client: Client,
    cache: Cache,
}

impl NostrNetworkHelper {
    // MARK: - Initialization

    pub async fn new(relay_url: String) -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::new(&Keys::generate());
        client.add_relay(relay_url.clone()).await?;
        client.connect().await;
        
        Ok(NostrNetworkHelper { client, cache: Cache::new(CACHE_MAX_AGE) })
    }

    // MARK: - Answering questions about a user

    pub async fn should_mute_notification_for_pubkey(
        &mut self,
        event: &Event,
        pubkey: &PublicKey,
    ) -> bool {
        log::debug!(
            "Checking if event {:?} should be muted for pubkey {:?}",
            event,
            pubkey
        );
        if let Some(mute_list) = self.get_public_mute_list(pubkey).await {
            for muted_public_key in mute_list.public_keys {
                if event.pubkey == muted_public_key {
                    return true;
                }
            }
            for muted_event_id in mute_list.event_ids {
                if event.id == muted_event_id
                    || event.referenced_event_ids().contains(&muted_event_id)
                {
                    return true;
                }
            }
            for muted_hashtag in mute_list.hashtags {
                if event
                    .referenced_hashtags()
                    .iter()
                    .any(|t| t == &muted_hashtag)
                {
                    return true;
                }
            }
            for muted_word in mute_list.words {
                if event
                    .content
                    .to_lowercase()
                    .contains(&muted_word.to_lowercase())
                {
                    return true;
                }
            }
        }
        false
    }

    pub async fn does_pubkey_follow_pubkey(
        &mut self,
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

    pub async fn get_public_mute_list(&mut self, pubkey: &PublicKey) -> Option<MuteList> {
        match self.cache.get_mute_list(pubkey) {
            Ok(optional_mute_list) => optional_mute_list,
            Err(_) => {
                // We don't have an answer from the cache, so we need to fetch it
                let mute_list_event = self.fetch_single_event(pubkey, Kind::MuteList)
                    .await;
                self.cache.add_optional_mute_list_with_author(pubkey, mute_list_event.clone());
                mute_list_event?.to_mute_list()
            }
        }
    }

    pub async fn get_contact_list(&mut self, pubkey: &PublicKey) -> Option<Event> {
        match self.cache.get_contact_list(pubkey) {
            Ok(optional_contact_list) => optional_contact_list,
            Err(_) => {
                // We don't have an answer from the cache, so we need to fetch it
                let contact_list_event = self.fetch_single_event(pubkey, Kind::ContactList)
                    .await;
                self.cache.add_optional_contact_list_with_author(pubkey, contact_list_event.clone());
                contact_list_event
            }
        }
    }

    // MARK: - Lower level fetching functions

    async fn fetch_single_event(&self, author: &PublicKey, kind: Kind) -> Option<Event> {
        let subscription_filter = Filter::new()
            .kinds(vec![kind])
            .authors(vec![author.clone()])
            .limit(1);

        let this_subscription_id = self
            .client
            .subscribe(Vec::from([subscription_filter]), None)
            .await;

        let mut event: Option<Event> = None;
        let mut notifications = self.client.notifications();

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

        self.client.unsubscribe(this_subscription_id).await;
        event
    }
}
