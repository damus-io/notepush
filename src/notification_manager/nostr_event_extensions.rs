use nostr_sdk::prelude::{MuteList, RelayMetadata};
use nostr_sdk::{Event, EventId, Kind, PublicKey, TagKind, TagStandard, Timestamp, Url};

/// Temporary scaffolding of old methods that have not been ported to use native Event methods
pub trait ExtendedEvent {
    /// Retrieves a set of pubkeys referenced by the note
    fn referenced_pubkeys(&self) -> std::collections::HashSet<PublicKey>;

    /// Retrieves a set of pubkeys relevant to the note
    fn relevant_pubkeys(&self) -> std::collections::HashSet<PublicKey>;

    /// Retrieves a set of event IDs referenced by the note
    fn referenced_event_ids(&self) -> std::collections::HashSet<EventId>;

    /// Retrieves a set of hashtags (t tags) referenced by the note
    fn referenced_hashtags(&self) -> std::collections::HashSet<String>;

    /// The unique id of this event (for notification tracking)
    // I.e. for live streams only notify once when first seen
    // also for articles we should notify when they are first posted only
    fn notification_id(&self) -> String;
}

// This is a wrapper around the Event type from strfry-policies, which adds some useful methods
impl ExtendedEvent for Event {
    /// Retrieves a set of pubkeys referenced by the note
    fn referenced_pubkeys(&self) -> std::collections::HashSet<PublicKey> {
        self.tags.public_keys().cloned().collect()
    }

    /// Retrieves a set of pubkeys relevant to the note
    fn relevant_pubkeys(&self) -> std::collections::HashSet<PublicKey> {
        let mut pubkeys = self.referenced_pubkeys();
        pubkeys.insert(self.pubkey);
        pubkeys
    }

    /// Retrieves a set of event IDs referenced by the note
    fn referenced_event_ids(&self) -> std::collections::HashSet<EventId> {
        self.tags.event_ids().cloned().collect()
    }

    /// Retrieves a set of hashtags (t tags) referenced by the note
    fn referenced_hashtags(&self) -> std::collections::HashSet<String> {
        self.tags.hashtags().map(|t| t.to_string()).collect()
    }

    fn notification_id(&self) -> String {
        if self.kind.is_addressable() {
            format!(
                "{}:{}:{}",
                self.kind.as_u16(),
                self.pubkey.to_hex(),
                self.tags.identifier().unwrap_or("")
            )
        } else if self.kind.is_replaceable() {
            format!("{}:{}", self.kind.as_u16(), self.pubkey)
        } else {
            self.id.to_sql_string()
        }
    }
}

// MARK: - SQL String Convertible

pub trait SqlStringConvertible {
    fn to_sql_string(&self) -> String;
    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        Self: Sized;
}

impl SqlStringConvertible for EventId {
    fn to_sql_string(&self) -> String {
        self.to_hex()
    }

    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        EventId::from_hex(&s).map_err(|e| e.into())
    }
}

impl SqlStringConvertible for PublicKey {
    fn to_sql_string(&self) -> String {
        self.to_hex()
    }

    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        PublicKey::from_hex(&s).map_err(|e| e.into())
    }
}

impl SqlStringConvertible for Timestamp {
    fn to_sql_string(&self) -> String {
        self.as_u64().to_string()
    }

    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let u64_timestamp: u64 = s.parse()?;
        Ok(Timestamp::from(u64_timestamp))
    }
}

pub trait MaybeConvertibleToMuteList {
    fn to_mute_list(&self) -> Option<MuteList>;
}

pub trait MaybeConvertibleToTimestampedMuteList {
    fn to_timestamped_mute_list(&self) -> Option<TimestampedMuteList>;
}

impl MaybeConvertibleToMuteList for Event {
    fn to_mute_list(&self) -> Option<MuteList> {
        if self.kind != Kind::MuteList {
            return None;
        }
        Some(MuteList {
            public_keys: self.referenced_pubkeys().iter().copied().collect(),
            hashtags: self.referenced_hashtags().iter().cloned().collect(),
            event_ids: self.referenced_event_ids().iter().copied().collect(),
            words: self
                .tags
                .filter(TagKind::Word)
                .map(|t| t.content().unwrap().to_string())
                .collect(),
        })
    }
}

impl MaybeConvertibleToTimestampedMuteList for Event {
    fn to_timestamped_mute_list(&self) -> Option<TimestampedMuteList> {
        if self.kind != Kind::MuteList {
            return None;
        }
        let mute_list = self.to_mute_list()?;
        Some(TimestampedMuteList {
            mute_list,
            timestamp: self.created_at,
        })
    }
}

pub type RelayList = Vec<(Url, Option<RelayMetadata>)>;

pub trait MaybeConvertibleToRelayList {
    fn to_relay_list(&self) -> Option<RelayList>;
}

impl MaybeConvertibleToRelayList for Event {
    fn to_relay_list(&self) -> Option<RelayList> {
        if self.kind != Kind::RelayList {
            return None;
        }
        // Convert the extracted relay list data fully into owned data that can be returned
        let extracted_relay_list_owned = self
            .tags
            .iter()
            .filter_map(|tag| {
                if let Some(TagStandard::RelayMetadata {
                    relay_url,
                    metadata,
                }) = tag.as_standardized()
                {
                    Some((relay_url.clone().into(), metadata.clone()))
                } else {
                    None
                }
            })
            .collect();

        Some(extracted_relay_list_owned)
    }
}

/// A trait for types that can be encoded to and decoded from JSON, specific to this crate.
/// This is defined to overcome the rust compiler's limitation of implementing a trait for a type that is not defined in the same crate.
pub trait Codable {
    fn to_json(&self) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;
    fn from_json(json: serde_json::Value) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        Self: Sized;
}

impl Codable for MuteList {
    fn to_json(&self) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Ok(serde_json::json!({
            "public_keys": self.public_keys.iter().map(|pk| pk.to_hex()).collect::<Vec<String>>(),
            "hashtags": self.hashtags.clone(),
            "event_ids": self.event_ids.iter().map(|id| id.to_hex()).collect::<Vec<String>>(),
            "words": self.words.clone()
        }))
    }

    fn from_json(json: serde_json::Value) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        Self: Sized,
    {
        let public_keys = json
            .get("public_keys")
            .ok_or_else(|| "Missing 'public_keys' field".to_string())?
            .as_array()
            .ok_or_else(|| "'public_keys' must be an array".to_string())?
            .iter()
            .map(|pk| {
                PublicKey::from_hex(pk.as_str().unwrap_or_default()).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<PublicKey>, String>>()?;

        let hashtags = json
            .get("hashtags")
            .ok_or_else(|| "Missing 'hashtags' field".to_string())?
            .as_array()
            .ok_or_else(|| "'hashtags' must be an array".to_string())?
            .iter()
            .map(|tag| {
                tag.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| "Invalid hashtag".to_string())
            })
            .collect::<Result<Vec<String>, String>>()?;

        let event_ids = json
            .get("event_ids")
            .ok_or_else(|| "Missing 'event_ids' field".to_string())?
            .as_array()
            .ok_or_else(|| "'event_ids' must be an array".to_string())?
            .iter()
            .map(|id| EventId::from_hex(id.as_str().unwrap_or_default()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<EventId>, String>>()?;

        let words = json
            .get("words")
            .ok_or_else(|| "Missing 'words' field".to_string())?
            .as_array()
            .ok_or_else(|| "'words' must be an array".to_string())?
            .iter()
            .map(|word| {
                word.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| "Invalid word".to_string())
            })
            .collect::<Result<Vec<String>, String>>()?;

        Ok(MuteList {
            public_keys,
            hashtags,
            event_ids,
            words,
        })
    }
}

#[derive(Clone)]
pub struct TimestampedMuteList {
    pub mute_list: MuteList,
    pub timestamp: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use {Event, PublicKey};

    #[test]
    fn test_relevant_pubkeys() {
        // Define the event data based on the provided JSON
        let event_data = r#"{"kind":9735,"tags":[["p","4d4fb5ff0afb8c04e6c6e03f51281b664576f985e5bc34a3a7ee310a1e821f47"],["e","7ef9165e1d68424b5e34134ecaa47411863f736f55a0c08f3a00db517fa15507"],["bolt11","lnbc19710n1pnwg0kdpp57003xutqz8pp9yhjwju243gdgelskndj2prt7gfhkdvmskp24r8qhp5ulu3sphjfgt8tdasqsptaz5xuxcvtmq7fhzdnmk5y3rpzwv6huwqcqzzsxqrrs0sp549znyngm55n9gpsexy0d92zvcqasqe5d0k7rdg4s66u2sez7fdhq9qyyssqpwd7ar8yez6k2yymn07z3ejkfxjzw4rld80jgq740hsszwk5m4nnh2hvx74nqs5cvwysafjlu5uu6p9t9heuqk6tdjkz3fpmj32raxsppy7ync"],["description","{\"id\":\"cdf701cd336d74ae2a234b9b3490a8e641575f1995b1a874dc32a42666454d11\",\"pubkey\":\"32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245\",\"created_at\":1726234316,\"kind\":9734,\"tags\":[[\"e\",\"7ef9165e1d68424b5e34134ecaa47411863f736f55a0c08f3a00db517fa15507\"],[\"p\",\"4d4fb5ff0afb8c04e6c6e03f51281b664576f985e5bc34a3a7ee310a1e821f47\"],[\"relays\",\"wss:\/\/nos.lol\",\"wss:\/\/theforest.nostr1.com\",\"wss:\/\/relay.damus.io\",\"wss:\/\/nostr.wine\",\"ws:\/\/monad.jb55.com:8080\",\"wss:\/\/relay.mostr.pub\"]],\"content\":\"\",\"sig\":\"859afb22644588574fa7002dac79a9fb6a4eb2715d494614578854420b9c3e6f7a3034baf2f6bc99b2baecbfcf22dad32cfd44dfa5b42ddd0e0c6198635aa0c1\"}"],["P","32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245"]],"created_at":1726234322,"content":"","sig":"3c9bb7553278e444addc591ee3565657a8b783d47a4438cadbed6ed1ae448d470e4d2471acd21a7ab26d7a2af8c102538f00e21bdcaabf213a5fceed2895e27a","id":"6901102ac61ecfb3a051acdd2103f0b4ea7cbe4dcc58bee47ce7d8b621cd5a7b","pubkey":"f81611363554b64306467234d7396ec88455707633f54738f6c4683535098cd3"}"#;

        // Parse the event from JSON
        let event: Event =
            serde_json::from_str(event_data).expect("Event data should be valid JSON");

        // Obtain relevant pubkeys
        let relevant_pubkeys = event.relevant_pubkeys();

        // Expected pubkey
        let expected_pubkey =
            PublicKey::from_hex("4d4fb5ff0afb8c04e6c6e03f51281b664576f985e5bc34a3a7ee310a1e821f47")
                .expect("Should be valid hex");
        let unexpected_pubkey =
            PublicKey::from_hex("32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245")
                .expect("Should be valid hex");

        assert!(!relevant_pubkeys.contains(&unexpected_pubkey));
        assert!(relevant_pubkeys.contains(&expected_pubkey));
    }
}
