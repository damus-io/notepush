use super::ExtendedEvent;
use nostr_sdk::prelude::*;

pub struct MuteManager {
    relay_url: String,
    client: Client,
}

impl MuteManager {
    pub async fn new(relay_url: String) -> Result<Self, Box<dyn std::error::Error>> {
        let client = Client::new(&Keys::generate());
        client.add_relay(relay_url.clone()).await?;
        client.connect().await;
        Ok(MuteManager { relay_url, client })
    }

    pub async fn should_mute_notification_for_pubkey(
        &self,
        event: &Event,
        pubkey: &PublicKey,
    ) -> bool {
        if let Some(mute_list) = self.get_public_mute_list(pubkey).await {
            for tag in mute_list.tags() {
                match tag.kind() {
                    TagKind::SingleLetter(SingleLetterTag {
                        character: Alphabet::P,
                        uppercase: false,
                    }) => {
                        let tagged_pubkey: Option<PublicKey> =
                            tag.content().and_then(|h| PublicKey::from_hex(h).ok());
                        if let Some(tagged_pubkey) = tagged_pubkey {
                            if event.pubkey == tagged_pubkey {
                                return true;
                            }
                        }
                    }
                    TagKind::SingleLetter(SingleLetterTag {
                        character: Alphabet::E,
                        uppercase: false,
                    }) => {
                        let tagged_event_id: Option<EventId> =
                            tag.content().and_then(|h| EventId::from_hex(h).ok());
                        if let Some(tagged_event_id) = tagged_event_id {
                            if event.id == tagged_event_id
                                || event.referenced_event_ids().contains(&tagged_event_id)
                            {
                                return true;
                            }
                        }
                    }
                    TagKind::SingleLetter(SingleLetterTag {
                        character: Alphabet::T,
                        uppercase: false,
                    }) => {
                        let tagged_hashtag: Option<String> = tag.content().map(|h| h.to_string());
                        if let Some(tagged_hashtag) = tagged_hashtag {
                            let tags_content =
                                event.get_tags_content(TagKind::SingleLetter(SingleLetterTag {
                                    character: Alphabet::T,
                                    uppercase: false,
                                }));
                            let should_mute = tags_content.iter().any(|t| t == &tagged_hashtag);
                            return should_mute;
                        }
                    }
                    TagKind::Word => {
                        let tagged_word: Option<String> = tag.content().map(|h| h.to_string());
                        if let Some(tagged_word) = tagged_word {
                            if event
                                .content
                                .to_lowercase()
                                .contains(&tagged_word.to_lowercase())
                            {
                                return true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        false
    }

    pub async fn get_public_mute_list(&self, pubkey: &PublicKey) -> Option<Event> {
        let subscription_filter = Filter::new()
            .kinds(vec![Kind::MuteList])
            .authors(vec![pubkey.clone()])
            .limit(1);

        let this_subscription_id = self
            .client
            .subscribe(Vec::from([subscription_filter]), None)
            .await;

        let mut mute_list: Option<Event> = None;
        let mut notifications = self.client.notifications();
        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event {
                subscription_id,
                event,
                ..
            } = notification
            {
                if this_subscription_id == subscription_id && event.kind == Kind::MuteList {
                    mute_list = Some((*event).clone());
                    break;
                }
            }
        }

        self.client.unsubscribe(this_subscription_id).await;
        mute_list
    }
}
