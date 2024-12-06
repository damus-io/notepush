use nostr::{self, key::PublicKey, nips::{nip51::MuteList, nip65}, Alphabet, SingleLetterTag, TagKind::SingleLetter};
use nostr_sdk::{EventId, Kind, TagKind};

/// Temporary scaffolding of old methods that have not been ported to use native Event methods
pub trait ExtendedEvent {
    /// Retrieves a set of pubkeys referenced by the note
    fn referenced_pubkeys(&self) -> std::collections::HashSet<nostr::PublicKey>;

    /// Retrieves a set of pubkeys relevant to the note
    fn relevant_pubkeys(&self) -> std::collections::HashSet<nostr::PublicKey>;

    /// Retrieves a set of event IDs referenced by the note
    fn referenced_event_ids(&self) -> std::collections::HashSet<nostr::EventId>;

    /// Retrieves a set of hashtags (t tags) referenced by the note
    fn referenced_hashtags(&self) -> std::collections::HashSet<String>;
}

// This is a wrapper around the Event type from strfry-policies, which adds some useful methods
impl ExtendedEvent for nostr::Event {
    /// Retrieves a set of pubkeys referenced by the note
    fn referenced_pubkeys(&self) -> std::collections::HashSet<nostr::PublicKey> {
        self.get_tags_content(SingleLetter(SingleLetterTag::lowercase(Alphabet::P)))
            .iter()
            .filter_map(|tag| PublicKey::from_hex(tag).ok())
            .collect()
    }

    /// Retrieves a set of pubkeys relevant to the note
    fn relevant_pubkeys(&self) -> std::collections::HashSet<nostr::PublicKey> {
        let mut pubkeys = self.referenced_pubkeys();
        pubkeys.insert(self.pubkey.clone());
        pubkeys
    }

    /// Retrieves a set of event IDs referenced by the note
    fn referenced_event_ids(&self) -> std::collections::HashSet<nostr::EventId> {
        self.get_tag_content(SingleLetter(SingleLetterTag::lowercase(Alphabet::E)))
            .iter()
            .filter_map(|tag| nostr::EventId::from_hex(tag).ok())
            .collect()
    }

    /// Retrieves a set of hashtags (t tags) referenced by the note
    fn referenced_hashtags(&self) -> std::collections::HashSet<String> {
        self.get_tags_content(SingleLetter(SingleLetterTag::lowercase(Alphabet::T)))
            .iter()
            .map(|tag| tag.to_string())
            .collect()
    }
}

// MARK: - SQL String Convertible

pub trait SqlStringConvertible {
    fn to_sql_string(&self) -> String;
    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error>>
    where
        Self: Sized;
}

impl SqlStringConvertible for nostr::EventId {
    fn to_sql_string(&self) -> String {
        self.to_hex()
    }

    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error>> {
        nostr::EventId::from_hex(s).map_err(|e| e.into())
    }
}

impl SqlStringConvertible for nostr::PublicKey {
    fn to_sql_string(&self) -> String {
        self.to_hex()
    }

    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error>> {
        nostr::PublicKey::from_hex(s).map_err(|e| e.into())
    }
}

impl SqlStringConvertible for nostr::Timestamp {
    fn to_sql_string(&self) -> String {
        self.as_u64().to_string()
    }

    fn from_sql_string(s: String) -> Result<Self, Box<dyn std::error::Error>> {
        let u64_timestamp: u64 = s.parse()?;
        Ok(nostr::Timestamp::from(u64_timestamp))
    }
}

pub trait MaybeConvertibleToMuteList {
    fn to_mute_list(&self) -> Option<MuteList>;
}

pub trait MaybeConvertibleToTimestampedMuteList {
    fn to_timestamped_mute_list(&self) -> Option<TimestampedMuteList>;
}

impl MaybeConvertibleToMuteList for nostr::Event {
    fn to_mute_list(&self) -> Option<MuteList> {
        if self.kind != Kind::MuteList {
            return None;
        }
        Some(MuteList {
            public_keys: self.referenced_pubkeys().iter().map(|pk| pk.clone()).collect(),
            hashtags: self.referenced_hashtags().iter().map(|tag| tag.clone()).collect(),
            event_ids: self.referenced_event_ids().iter().map(|id| id.clone()).collect(),
            words: self.get_tags_content(TagKind::Word).iter().map(|tag| tag.to_string()).collect(),
        })
    }
}

impl MaybeConvertibleToTimestampedMuteList for nostr::Event {
    fn to_timestamped_mute_list(&self) -> Option<TimestampedMuteList> {
        if self.kind != Kind::MuteList {
            return None;
        }
        let mute_list = self.to_mute_list()?;
        Some(TimestampedMuteList {
            mute_list,
            timestamp: self.created_at.clone(),
        })
    }
}

pub type RelayList = Vec<(nostr::Url, Option<nostr::nips::nip65::RelayMetadata>)>;

pub trait MaybeConvertibleToRelayList {
    fn to_relay_list(&self) -> Option<RelayList>;
}

impl MaybeConvertibleToRelayList for nostr::Event {
    fn to_relay_list(&self) -> Option<RelayList> {
        if self.kind != Kind::RelayList {
            return None;
        }
        let extracted_relay_list = nip65::extract_relay_list(&self);
        // Convert the extracted relay list data fully into owned data that can be returned
        let extracted_relay_list_owned = extracted_relay_list.into_iter()
            .map(|(url, metadata)| (url.clone(), metadata.as_ref().map(|m| m.clone())))
            .collect();

        Some(extracted_relay_list_owned)
    }
}

/// A trait for types that can be encoded to and decoded from JSON, specific to this crate.
/// This is defined to overcome the rust compiler's limitation of implementing a trait for a type that is not defined in the same crate.
pub trait Codable {
    fn to_json(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>>;
    fn from_json(json: serde_json::Value) -> Result<Self, Box<dyn std::error::Error>>
    where
        Self: Sized;
}

impl Codable for MuteList {
    fn to_json(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        Ok(serde_json::json!({
            "public_keys": self.public_keys.iter().map(|pk| pk.to_hex()).collect::<Vec<String>>(),
            "hashtags": self.hashtags.clone(),
            "event_ids": self.event_ids.iter().map(|id| id.to_hex()).collect::<Vec<String>>(),
            "words": self.words.clone()
        }))
    }

    fn from_json(json: serde_json::Value) -> Result<Self, Box<dyn std::error::Error>>
    where
        Self: Sized {
            let public_keys = json.get("public_keys")
                .ok_or_else(|| "Missing 'public_keys' field".to_string())?
                .as_array()
                .ok_or_else(|| "'public_keys' must be an array".to_string())?
                .iter()
                .map(|pk| PublicKey::from_hex(pk.as_str().unwrap_or_default()).map_err(|e| e.to_string()))
                .collect::<Result<Vec<PublicKey>, String>>()?;

            let hashtags = json.get("hashtags")
                .ok_or_else(|| "Missing 'hashtags' field".to_string())?
                .as_array()
                .ok_or_else(|| "'hashtags' must be an array".to_string())?
                .iter()
                .map(|tag| tag.as_str().map(|s| s.to_string()).ok_or_else(|| "Invalid hashtag".to_string()))
                .collect::<Result<Vec<String>, String>>()?;

            let event_ids = json.get("event_ids")
                .ok_or_else(|| "Missing 'event_ids' field".to_string())?
                .as_array()
                .ok_or_else(|| "'event_ids' must be an array".to_string())?
                .iter()
                .map(|id| EventId::from_hex(id.as_str().unwrap_or_default()).map_err(|e| e.to_string()))
                .collect::<Result<Vec<EventId>, String>>()?;

            let words = json.get("words")
                .ok_or_else(|| "Missing 'words' field".to_string())?
                .as_array()
                .ok_or_else(|| "'words' must be an array".to_string())?
                .iter()
                .map(|word| word.as_str().map(|s| s.to_string()).ok_or_else(|| "Invalid word".to_string()))
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
    pub timestamp: nostr::Timestamp,
}
