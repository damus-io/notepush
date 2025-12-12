//! Event filtering integration with noteguard-core.
//!
//! This module provides DoS protection by filtering incoming events before
//! they trigger expensive operations like remote mute list lookups.
//!
//! ## Why filter events?
//!
//! Without filtering, an attacker could spam the relay with events that:
//! 1. Pass basic validation (valid signatures, recent timestamps)
//! 2. Trigger expensive remote fetches (mute lists, follow lists)
//! 3. Overwhelm the relay with network I/O
//!
//! By filtering events BEFORE notification processing, we reject spam early
//! and protect the relay from resource exhaustion.

use nostr::Event;
use noteguard_core::{Action, Config, InputMessage, Note, Noteguard, OutputMessage};
use std::sync::Mutex;

/// Wrapper around Noteguard that provides thread-safe event filtering.
///
/// The Noteguard instance maintains state (e.g., rate limit token buckets),
/// so it must be shared via Mutex to ensure consistent filtering across
/// concurrent WebSocket connections.
pub struct EventFilter {
    /// The underlying noteguard filter pipeline, protected by mutex
    /// because filters like RateLimit maintain mutable state.
    inner: Mutex<Noteguard>,
}

impl EventFilter {
    /// Creates a new EventFilter with the given configuration.
    ///
    /// Returns None if configuration loading fails, allowing the relay
    /// to operate without filtering (with a warning log).
    pub fn new(config: &Config) -> Option<Self> {
        let mut noteguard = Noteguard::new();

        if let Err(e) = noteguard.load_config(config) {
            log::error!("failed to load noteguard config: {}", e);
            return None;
        }

        Some(EventFilter {
            inner: Mutex::new(noteguard),
        })
    }

    /// Creates an EventFilter with an empty pipeline (accepts all events).
    ///
    /// Useful as a fallback when no configuration is provided.
    pub fn permissive() -> Self {
        EventFilter {
            inner: Mutex::new(Noteguard::new()),
        }
    }

    /// Filters an event, returning the filter decision.
    ///
    /// The `source_info` parameter identifies the event source for rate limiting.
    /// Typically this is the client IP, but for authenticated connections it
    /// could be the pubkey (which is more appropriate for per-user limits).
    pub fn filter_event(&self, event: &Event, source_info: &str) -> FilterResult {
        let input = event_to_input_message(event, source_info);

        let output = {
            // Hold lock only for the duration of the filter operation
            let mut guard = self.inner.lock().expect("filter mutex poisoned");
            guard.run(input)
        };

        FilterResult::from_output(output)
    }
}

/// Result of filtering an event.
#[derive(Debug, Clone)]
pub enum FilterResult {
    /// Event passed all filters and should be processed
    Accept,
    /// Event was rejected - return this message to the client
    Reject(String),
    /// Event was shadow-rejected - pretend to accept but don't process
    ShadowReject,
}

impl FilterResult {
    fn from_output(output: OutputMessage) -> Self {
        match output.action {
            Action::Accept => FilterResult::Accept,
            Action::Reject => {
                let msg = output
                    .msg
                    .unwrap_or_else(|| "blocked by filter".to_string());
                FilterResult::Reject(msg)
            }
            Action::ShadowReject => FilterResult::ShadowReject,
        }
    }

    /// Returns true if the event should be processed (Accept only).
    pub fn should_process(&self) -> bool {
        matches!(self, FilterResult::Accept)
    }
}

/// Converts a nostr::Event to the noteguard InputMessage format.
///
/// This bridges the nostr crate's Event type to noteguard-core's Note type.
fn event_to_input_message(event: &Event, source_info: &str) -> InputMessage {
    // Convert tags from nostr format to noteguard format.
    // nostr::Tag implements AsRef<[TagStandard]> but we need to serialize
    // to the raw JSON array format that noteguard expects.
    let tags: Vec<Vec<String>> = event
        .tags
        .iter()
        .map(|tag| {
            // Convert tag to JSON and extract the string array
            // This handles all tag types correctly
            if let Ok(json_val) = serde_json::to_value(tag) {
                if let Some(arr) = json_val.as_array() {
                    return arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
            }
            Vec::new()
        })
        .collect();

    let note = Note {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        content: event.content.clone(),
        created_at: event.created_at.as_u64() as i64,
        kind: event.kind.as_u16() as i64,
        tags,
        sig: event.sig.to_string(),
    };

    InputMessage {
        message_type: "new".to_string(),
        event: note,
        received_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        source_type: "websocket".to_string(),
        source_info: source_info.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys};

    fn create_test_event() -> Event {
        let keys = Keys::generate();
        EventBuilder::text_note("test content", [])
            .to_event(&keys)
            .unwrap()
    }

    #[test]
    fn test_permissive_filter_accepts_all() {
        let filter = EventFilter::permissive();
        let event = create_test_event();

        let result = filter.filter_event(&event, "127.0.0.1");
        assert!(matches!(result, FilterResult::Accept));
    }

    #[test]
    fn test_blacklist_rejects_pubkey() {
        let event = create_test_event();
        let pubkey_hex = event.pubkey.to_hex();

        let config: Config = toml::from_str(&format!(
            r#"
            pipeline = ["blacklist"]
            [filters.blacklist]
            pubkeys = ["{}"]
            "#,
            pubkey_hex
        ))
        .unwrap();

        let filter = EventFilter::new(&config).unwrap();
        let result = filter.filter_event(&event, "127.0.0.1");

        match result {
            FilterResult::Reject(msg) => assert!(msg.contains("blacklist")),
            _ => panic!("expected Reject, got {:?}", result),
        }
    }

    #[test]
    fn test_conversion_preserves_event_data() {
        let event = create_test_event();
        let input = event_to_input_message(&event, "test-source");

        assert_eq!(input.event.id, event.id.to_hex());
        assert_eq!(input.event.pubkey, event.pubkey.to_hex());
        assert_eq!(input.event.content, event.content);
        assert_eq!(input.event.kind, event.kind.as_u16() as i64);
        assert_eq!(input.source_info, "test-source");
    }
}
