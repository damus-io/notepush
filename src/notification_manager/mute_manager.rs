use std::collections::HashSet;
use tokio::sync::mpsc;

pub struct MuteManager {
    relay_url: String,
    client: Option<Client>,
}

impl MuteManager {
    pub fn new(relay_url: String) -> Self {
        let mut manager = MuteManager {
            relay_url,
            client: None,
        };
        tokio::spawn(async move {
            let client = Client::new(&Keys::generate());
            client.add_relay(manager.relay_url.clone()).await.unwrap();
            client.connect().await;
            manager.client = Some(client);
        });
        manager
    }

    pub async fn should_mute_notification_for_pubkey(&self, event: &Event, pubkey: &str) -> bool {
        if let Some(mute_list) = self.get_public_mute_list(pubkey).await {
            for tag in mute_list.tags() {
                match tag.as_slice() {
                    ["p", muted_pubkey] => {
                        if event.pubkey == *muted_pubkey {
                            return true;
                        }
                    }
                    ["e", muted_event_id] => {
                        if event.id == *muted_event_id || event.referenced_event_ids().contains(muted_event_id) {
                            return true;
                        }
                    }
                    ["t", muted_hashtag] => {
                        if event.tags.iter().any(|t| t.to_vec().as_slice() == ["t", muted_hashtag]) {
                            return true;
                        }
                    }
                    ["word", muted_word] => {
                        if event.content.to_lowercase().contains(&muted_word.to_lowercase()) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        false
    }

    pub async fn get_public_mute_list(&self, pubkey: &str) -> Option<Event> {
        if let Some(client) = &self.client {
            let (tx, mut rx) = mpsc::channel(100);

            let subscription = Filter::new()
                .kinds(vec![Kind::MuteList])
                .authors(vec![pubkey.to_string()])
                .limit(1);

            client.subscribe(vec![subscription]).await;

            let mut mute_lists = Vec::new();

            while let Some(notification) = rx.recv().await {
                if let RelayPoolNotification::Event(_url, event) = notification {
                    mute_lists.push(event);
                    break;
                }
            }

            client.unsubscribe().await;

            mute_lists.into_iter().next()
        } else {
            None
        }
    }
}

trait EventExt {
    fn referenced_event_ids(&self) -> HashSet<String>;
}

impl EventExt for Event {
    fn referenced_event_ids(&self) -> HashSet<String> {
        self.tags
            .iter()
            .filter(|tag| tag.first() == Some(&"e".to_string()))
            .filter_map(|tag| tag.get(1).cloned())
            .collect()
    }
}
