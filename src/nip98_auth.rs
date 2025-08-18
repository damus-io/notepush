use base64::prelude::*;
use nostr_sdk::hashes::{sha256, Hash};
use nostr_sdk::prelude::hex;
use nostr_sdk::{Event, JsonUtil, Kind, TagKind, Timestamp};
use std::borrow::Cow;

pub fn nip98_verify_auth_header(
    auth_header: &str,
    url: &str,
    method: &str,
    body: &Option<Vec<u8>>,
) -> Result<Event, &'static str> {
    if auth_header.is_empty() {
        return Err("Nostr authorization header missing");
    }

    let auth_header_parts: Vec<&str> = auth_header.split_whitespace().collect();
    if auth_header_parts.len() != 2 {
        return Err("Nostr authorization header does not have 2 parts");
    }

    if auth_header_parts[0] != "Nostr" {
        return Err("Nostr authorization header does not start with `Nostr`");
    }

    let base64_encoded_note = auth_header_parts[1];
    if base64_encoded_note.is_empty() {
        return Err("Nostr authorization header does not have a base64 encoded note");
    }

    let decoded_note_json = BASE64_STANDARD
        .decode(base64_encoded_note.as_bytes())
        .map_err(|_| "Failed to decode base64 encoded note from Nostr authorization header")?;

    let note =
        Event::from_json(decoded_note_json).map_err(|_| "Could not parse Nostr note from JSON")?;

    if note.kind != Kind::HttpAuth {
        return Err("Nostr note kind in authorization header is incorrect");
    }

    let authorized_url = note
        .tags
        .find(TagKind::Custom(Cow::Borrowed("u")))
        .and_then(|t| t.content())
        .ok_or_else(|| "Missing 'u' tag from Nostr authorization header")?;

    let authorized_method = note
        .tags
        .find(TagKind::Method)
        .and_then(|t| t.content())
        .ok_or_else(|| "Missing 'method' tag from Nostr authorization header")?;

    if authorized_url != url || authorized_method != method {
        log::warn!(
            "Auth mismatch: method: {}<>{}, url: {}<>{}",
            authorized_method,
            method,
            authorized_url,
            url
        );
        return Err("Auth note url and/or method does not match request");
    }

    let current_time = Timestamp::now().as_u64();
    let time_delta = note.created_at.as_u64().abs_diff(current_time);
    if time_delta > 60 {
        log::warn!(
            "Auth timestamp out of range: Time delta: {} seconds",
            time_delta
        );
        return Err("Auth timestamp is out of range");
    }

    if let Some(body_data) = body {
        let authorized_content_hash_bytes: Vec<u8> = hex::decode(
            note.tags
                .find(TagKind::Payload)
                .and_then(|t| t.content())
                .ok_or("Missing 'payload' tag from Nostr authorization header")?,
        )
        .map_err(|_| "Failed to decode hex encoded payload from Nostr authorization header")?;

        let authorized_content_hash = sha256::Hash::from_slice(&authorized_content_hash_bytes)
            .map_err(|_| "Failed to convert hex encoded payload to Sha256Hash")?;

        let body_hash: sha256::Hash = Hash::hash(body_data);
        if authorized_content_hash != body_hash {
            return Err("Auth note payload hash does not match request body hash");
        }
    } else {
        let authorized_content_hash_string =
            note.tags.find(TagKind::Payload).and_then(|t| t.content());
        if authorized_content_hash_string.is_some() {
            return Err("Auth note has payload tag but request has no body");
        }
    }

    // Verify both the Event ID and the cryptographic signature
    if note.verify().is_err() {
        return Err("Auth note id or signature is invalid");
    }

    Ok(note)
}
