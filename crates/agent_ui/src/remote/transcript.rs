use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};

const CHUNK_JSON_BYTES: usize = 32_000;

pub(super) fn index(args: &Value, key: &str) -> Result<Option<usize>> {
    args.get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .with_context(|| format!("{key} must be a non-negative integer"))
        })
        .transpose()
}

pub(super) fn fingerprint(text: &str) -> String {
    super::BASE64.encode(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref())
}

pub(super) fn body_chunk(text: &str, args: &Value) -> Result<Value> {
    let offset = index(args, "offset")?.unwrap_or(0);
    let expected_total = index(args, "total_bytes")?;
    let expected_version = args
        .get("version")
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().context("version must be a string"))
        .transpose()?;
    if expected_total.is_some() != expected_version.is_some()
        || (offset > 0 && expected_version.is_none())
    {
        bail!("Continue with the version and total_bytes returned by the first chunk");
    }
    // Preserve a snapshot prefix while a live message appends more text. Any
    // rewrite of that prefix is a conflict, never mixed into already loaded text.
    let total = expected_total.unwrap_or(text.len());
    if total > text.len() || !text.is_char_boundary(total) {
        bail!("The message changed while loading. Refresh its content.");
    }
    let text = &text[..total];
    let version = fingerprint(text);
    if expected_version.is_some_and(|expected| expected != version) {
        bail!("The message changed while loading. Refresh its content.");
    }
    if offset > total || !text.is_char_boundary(offset) {
        bail!("The content offset is invalid. Refresh its content.");
    }
    let mut end = offset.saturating_add(CHUNK_JSON_BYTES).min(total);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    while super::json_len(&text[offset..end]) > CHUNK_JSON_BYTES {
        end = offset + (end - offset) / 2;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    Ok(json!({
        "offset": offset,
        "next_offset": end,
        "total_bytes": total,
        "version": version,
        "text": &text[offset..end],
        "done": end == total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_reassemble_all_text_including_unicode_and_control_characters() {
        let text = format!("  {}  \n", "🦀\\\"\n\t\0".repeat(20_000));
        let mut args = json!({});
        let mut collected = String::new();
        loop {
            let chunk = body_chunk(&text, &args).expect("chunk");
            assert!(chunk.to_string().len() < super::super::channel::MAX_ANSWER_LEN);
            collected.push_str(chunk["text"].as_str().expect("text"));
            if chunk["done"] == true {
                break;
            }
            assert!(chunk["next_offset"].as_u64() > chunk["offset"].as_u64());
            args = json!({
                "offset": chunk["next_offset"],
                "total_bytes": chunk["total_bytes"],
                "version": chunk["version"],
            });
        }
        assert_eq!(collected, text);
    }

    #[test]
    fn append_only_growth_keeps_a_snapshot_readable_but_rewrites_are_rejected() {
        let original = "a".repeat(70_000);
        let first = body_chunk(&original, &json!({})).expect("first chunk");
        let args = json!({
            "offset": first["next_offset"],
            "total_bytes": first["total_bytes"],
            "version": first["version"],
        });
        let grown = format!("{original}more text");
        let next = body_chunk(&grown, &args).expect("original prefix");
        assert_eq!(next["total_bytes"], original.len());
        assert_eq!(next["version"], first["version"]);
        assert!(body_chunk(&format!("changed{original}"), &args).is_err());
        assert!(body_chunk("shortened", &args).is_err());
    }

    #[test]
    fn malformed_and_non_boundary_offsets_fail_without_truncation() {
        for args in [
            json!({ "offset": -1 }),
            json!({ "offset": "0" }),
            json!({ "offset": 0.5 }),
            json!({ "offset": 1 }),
            json!({ "total_bytes": 1, "version": "v" }),
            json!({ "version": 4 }),
        ] {
            assert!(body_chunk("🦀", &args).is_err());
        }
        let empty = body_chunk("", &json!({})).expect("empty body");
        assert_eq!(empty["done"], true);
        assert_eq!(empty["total_bytes"], 0);
        assert_eq!(empty["text"], "");
    }
}
