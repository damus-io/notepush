use nostr::nips::nip51::MuteList;
use nostr_sdk::Event;
use super::nostr_event_extensions::ExtendedEvent;


pub fn should_mute_notification_for_mutelist(
        event: &Event,
        mute_list: &MuteList,
    ) -> bool {
    for muted_public_key in &mute_list.public_keys {
        if event.pubkey == *muted_public_key {
            return true;
        }
    }
    for muted_event_id in &mute_list.event_ids {
        if event.id == *muted_event_id
            || event.referenced_event_ids().contains(&muted_event_id)
        {
            return true;
        }
    }
    for muted_hashtag in &mute_list.hashtags {
        if event
            .referenced_hashtags()
            .iter()
            .any(|t| t == muted_hashtag)
        {
            return true;
        }
    }
    for muted_word in &mute_list.words {
        if event
            .content
            .to_lowercase()
            .contains(&muted_word.to_lowercase())
        {
            return true;
        }
    }
    false
}
