use base64::prelude::*;
use nostr;
use nostr::bitcoin::hashes::sha256::Hash as Sha256Hash;
use nostr::bitcoin::hashes::Hash;
use nostr::util::hex;
use nostr::Timestamp;
use serde_json::Value;

pub async fn nip98_verify_auth_header(
    auth_header: String,
    url: &str,
    method: &str,
    body: Option<&[u8]>,
) -> Result<nostr::PublicKey, String> {
    if auth_header.is_empty() {
        return Err("Nostr authorization header missing".to_string());
    }

    let auth_header_parts: Vec<&str> = auth_header.split_whitespace().collect();
    if auth_header_parts.len() != 2 {
        return Err("Nostr authorization header does not have 2 parts".to_string());
    }

    if auth_header_parts[0] != "Nostr" {
        return Err("Nostr authorization header does not start with `Nostr`".to_string());
    }

    let base64_encoded_note = auth_header_parts[1];
    if base64_encoded_note.is_empty() {
        return Err("Nostr authorization header does not have a base64 encoded note".to_string());
    }

    let decoded_note_json = BASE64_STANDARD
        .decode(base64_encoded_note.as_bytes())
        .map_err(|_| {
            format!("Failed to decode base64 encoded note from Nostr authorization header")
        })?;

    let note_value: Value = serde_json::from_slice(&decoded_note_json)
        .map_err(|_| format!("Could not parse JSON note from authorization header"))?;

    let note: nostr::Event = nostr::Event::from_value(note_value)
        .map_err(|_| format!("Could not parse Nostr note from JSON"))?;

    if note.kind != nostr::Kind::HttpAuth {
        return Err("Nostr note kind in authorization header is incorrect".to_string());
    }

    let authorized_url = note
        .get_tag_content(nostr::TagKind::SingleLetter(
            nostr::SingleLetterTag::lowercase(nostr::Alphabet::U),
        ))
        .ok_or_else(|| "Missing 'u' tag from Nostr authorization header".to_string())?;

    let authorized_method = note
        .get_tag_content(nostr::TagKind::Method)
        .ok_or_else(|| "Missing 'method' tag from Nostr authorization header".to_string())?;

    if authorized_url != url || authorized_method != method {
        return Err(format!(
            "Auth note url and/or method does not match request. Auth note url: {}; Request url: {}; Auth note method: {}; Request method: {}",
            authorized_url, url, authorized_method, method
        ));
    }

    let current_time: nostr::Timestamp = nostr::Timestamp::now();
    let note_created_at: nostr::Timestamp = note.created_at();
    let time_delta = TimeDelta::subtracting(current_time, note_created_at);
    if (time_delta.negative && time_delta.delta_abs_seconds > 30)
        || (!time_delta.negative && time_delta.delta_abs_seconds > 60)
    {
        return Err(format!(
            "Auth note is too old. Current time: {}; Note created at: {}; Time delta: {} seconds",
            current_time, note_created_at, time_delta
        ));
    }

    if let Some(body_data) = body {
        let authorized_content_hash_bytes: Vec<u8> = hex::decode(
            note.get_tag_content(nostr::TagKind::Payload)
                .ok_or("Missing 'payload' tag from Nostr authorization header")?,
        )
        .map_err(|_| {
            format!("Failed to decode hex encoded payload from Nostr authorization header")
        })?;

        let authorized_content_hash: Sha256Hash =
            Sha256Hash::from_slice(&authorized_content_hash_bytes)
                .map_err(|_| format!("Failed to convert hex encoded payload to Sha256Hash"))?;

        let body_hash = Sha256Hash::hash(body_data);
        if authorized_content_hash != body_hash {
            return Err("Auth note payload hash does not match request body hash".to_string());
        }
    } else {
        let authorized_content_hash_string = note.get_tag_content(nostr::TagKind::Payload);
        if authorized_content_hash_string.is_some() {
            return Err("Auth note has payload tag but request has no body".to_string());
        }
    }

    // Verify both the Event ID and the cryptographic signature
    if note.verify().is_err() {
        return Err("Auth note id or signature is invalid".to_string());
    }

    Ok(note.pubkey)
}

struct TimeDelta {
    delta_abs_seconds: u64,
    negative: bool,
}

impl TimeDelta {
    /// Safely calculate the difference between two timestamps in seconds
    /// This function is safer against overflows than subtracting the timestamps directly
    fn subtracting(t1: Timestamp, t2: Timestamp) -> TimeDelta {
        if t1 > t2 {
            TimeDelta {
                delta_abs_seconds: (t1 - t2).as_u64(),
                negative: false,
            }
        } else {
            TimeDelta {
                delta_abs_seconds: (t2 - t1).as_u64(),
                negative: true,
            }
        }
    }
}

impl std::fmt::Display for TimeDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.negative {
            write!(f, "-{}", self.delta_abs_seconds)
        } else {
            write!(f, "{}", self.delta_abs_seconds)
        }
    }
}
