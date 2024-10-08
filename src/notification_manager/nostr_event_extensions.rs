use nostr::{self, key::PublicKey, nips::nip51::MuteList, Alphabet, SingleLetterTag, TagKind::SingleLetter};
use nostr_sdk::{Kind, TagKind};

/// Temporary scaffolding of old methods that have not been ported to use native Event methods
pub trait ExtendedEvent {
    /// Checks if the note references a given pubkey
    fn references_pubkey(&self, pubkey: &PublicKey) -> bool;

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
    /// Checks if the note references a given pubkey
    fn references_pubkey(&self, pubkey: &PublicKey) -> bool {
        self.referenced_pubkeys().contains(pubkey)
    }

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
