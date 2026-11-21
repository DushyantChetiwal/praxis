use std::collections::HashMap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result, bail, ensure};
use gpui::{App, AppContext as _, Global, Task};
use image::{ImageFormat, ImageReader};
use serde_json::{Value, json};

use super::uploads::{
    ChunkBuffer, MAX_CHUNK_BYTES, MAX_PARALLEL_CHUNKS, negotiate_chunk_bytes,
};
use super::{crypto, required, transcript};

const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_IMAGES: usize = 4;
const MAX_BUFFERED_BYTES: usize = 16 * 1024 * 1024;
const EXPIRY: Duration = Duration::from_secs(300);

type UploadKey = (String, String);

struct Upload {
    client_id: String,
    session: String,
    mime: String,
    size: usize,
    chunk_bytes: usize,
    buffer: ChunkBuffer,
    content: Option<acp::ContentBlock>,
    updated: Instant,
}

#[derive(Default)]
struct Uploads(HashMap<UploadKey, Upload>);
impl Global for Uploads {}

impl Uploads {
    fn prune(&mut self) {
        self.0.retain(|_, upload| upload.updated.elapsed() < EXPIRY);
    }

    fn begin(&mut self, phone: &str, session: &str, args: &Value) -> Result<Value> {
        self.prune();
        let size = transcript::index(args, "size")?.context("expected image size")?;
        ensure!(
            (1..=MAX_IMAGE_BYTES).contains(&size),
            "Images must be at most 2 MiB. Choose a smaller image."
        );
        let mime = required(args, "mime_type")?;
        ensure!(
            matches!(mime, "image/png" | "image/jpeg" | "image/webp"),
            "Choose a PNG, JPEG, or WebP image."
        );
        let chunk_bytes = negotiate_chunk_bytes(args)?;
        let client_id = args.get("client_id").and_then(Value::as_str).unwrap_or("");
        if !client_id.is_empty() {
            uuid::Uuid::parse_str(client_id).context("expected an image client ID")?;
            if let Some(((_, id), upload)) = self
                .0
                .iter()
                .find(|((owner, _), upload)| owner == phone && upload.client_id == client_id)
            {
                ensure!(
                    upload.session == session && upload.mime == mime && upload.size == size,
                    "An image ID was reused with different upload metadata."
                );
                return Ok(json!({
                    "upload_id": id,
                    "chunk_bytes": upload.chunk_bytes,
                    "max_parallel": MAX_PARALLEL_CHUNKS,
                    "next_offset": upload.buffer.contiguous_offset(),
                    "received_bytes": upload.buffer.received_bytes(),
                    "ready": upload.content.is_some(),
                }));
            }
        }
        ensure!(
            self.0.keys().filter(|(owner, _)| owner == phone).count() < MAX_IMAGES,
            "Finish or remove an image upload before adding another."
        );
        let reserved: usize = self.0.values().map(|upload| upload.size).sum();
        ensure!(
            reserved.saturating_add(size) <= MAX_BUFFERED_BYTES,
            "Praxis has too many pending image uploads. Retry shortly."
        );
        let id = crypto::random_id()?;
        self.0.insert(
            (phone.into(), id.clone()),
            Upload {
                client_id: client_id.into(),
                session: session.into(),
                mime: mime.into(),
                size,
                chunk_bytes,
                buffer: ChunkBuffer::new(size),
                content: None,
                updated: Instant::now(),
            },
        );
        Ok(json!({
            "upload_id": id,
            "chunk_bytes": chunk_bytes,
            "max_parallel": MAX_PARALLEL_CHUNKS,
            "next_offset": 0,
            "received_bytes": 0,
            "ready": false,
        }))
    }

    fn get(&mut self, phone: &str, session: &str, id: &str) -> Result<&mut Upload> {
        self.prune();
        let upload = self
            .0
            .get_mut(&(phone.into(), id.into()))
            .context("That image upload expired or belongs to another phone. Attach it again.")?;
        ensure!(
            upload.session == session,
            "That image belongs to another conversation. Attach it again here."
        );
        Ok(upload)
    }

    fn chunk(&mut self, phone: &str, session: &str, args: &Value) -> Result<Value> {
        let id = required(args, "upload_id")?;
        let offset = transcript::index(args, "offset")?.context("expected image offset")?;
        let encoded = required(args, "data")?;
        ensure!(
            encoded.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4,
            "The image chunk is too large."
        );
        let bytes = crypto::decode(encoded)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CHUNK_BYTES,
            "The image chunk is empty or too large."
        );
        let upload = self.get(phone, session, id)?;
        ensure!(
            upload.content.is_none(),
            "That image upload is already complete."
        );
        upload.buffer.insert(offset, bytes)?;
        upload.updated = Instant::now();
        Ok(json!({
            "next_offset": upload.buffer.contiguous_offset(),
            "received_bytes": upload.buffer.received_bytes(),
        }))
    }

    fn contents(
        &mut self,
        phone: &str,
        session: &str,
        args: &Value,
    ) -> Result<Vec<acp::ContentBlock>> {
        let Some(value) = args.get("images") else {
            return Ok(Vec::new());
        };
        let ids = value.as_array().context("expected image upload IDs")?;
        ensure!(
            !ids.is_empty() && ids.len() <= MAX_IMAGES,
            "Attach between one and four images."
        );
        let mut used = std::collections::HashSet::new();
        let mut content = Vec::new();
        for value in ids {
            let id = value.as_str().context("expected an image upload ID")?;
            ensure!(used.insert(id), "An image was attached more than once.");
            content.push(
                self.get(phone, session, id)?
                    .content
                    .clone()
                    .context("An image is still uploading. Wait for it to finish.")?,
            );
        }
        Ok(content)
    }
}

fn uploads(cx: &mut App) -> &mut Uploads {
    if !cx.has_global::<Uploads>() {
        cx.set_global(Uploads::default());
    }
    cx.global_mut::<Uploads>()
}

pub(super) fn clear(phone: &str, cx: &mut App) {
    if cx.has_global::<Uploads>() {
        cx.global_mut::<Uploads>()
            .0
            .retain(|(owner, _), _| owner != phone);
    }
}

pub(super) fn contents(
    phone: &str,
    session: &str,
    args: &Value,
    cx: &mut App,
) -> Result<Vec<acp::ContentBlock>> {
    uploads(cx).contents(phone, session, args)
}

pub(super) fn consume(phone: &str, args: &Value, cx: &mut App) {
    if let Some(ids) = args.get("images").and_then(Value::as_array) {
        for id in ids.iter().filter_map(Value::as_str) {
            uploads(cx).0.remove(&(phone.into(), id.into()));
        }
    }
}

pub(super) fn handle(
    phone: &str,
    session: &str,
    op: &str,
    args: &Value,
    cx: &mut App,
) -> Task<Result<Value>> {
    let result = match op {
        "image_begin" => uploads(cx).begin(phone, session, args),
        "image_chunk" => uploads(cx).chunk(phone, session, args),
        "image_discard" => (|| {
            let id = required(args, "upload_id")?;
            uploads(cx).get(phone, session, id)?;
            uploads(cx).0.remove(&(phone.into(), id.into()));
            Ok(json!({ "discarded": true }))
        })(),
        "image_finish" => {
            let pending = (|| -> Result<_> {
                let id = required(args, "upload_id")?.to_string();
                let upload = uploads(cx).get(phone, session, &id)?;
                if upload.content.is_some() {
                    return Ok((id, None));
                }
                ensure!(
                    upload.buffer.is_complete(),
                    "The image upload is incomplete."
                );
                let bytes = upload.buffer.take_bytes()?;
                Ok((id, Some((bytes, upload.mime.clone()))))
            })();
            let (id, pending_validation) = match pending {
                Ok(pending) => pending,
                Err(error) => return Task::ready(Err(error)),
            };
            let Some((bytes, mime)) = pending_validation else {
                return Task::ready(Ok(json!({ "upload_id": id, "ready": true })));
            };
            let phone = phone.to_string();
            let session = session.to_string();
            return cx.spawn(async move |cx| {
                let validated = cx
                    .background_spawn(async move { validate(bytes, &mime) })
                    .await;
                cx.update(|cx| {
                    let content = match validated {
                        Ok(content) => content,
                        Err(error) => {
                            uploads(cx).0.remove(&(phone.clone(), id.clone()));
                            return Err(error);
                        }
                    };
                    let upload = uploads(cx).get(&phone, &session, &id)?;
                    upload.content = Some(content);
                    upload.updated = Instant::now();
                    Ok(json!({ "upload_id": id, "ready": true }))
                })
            });
        }
        _ => Err(anyhow::anyhow!("Unknown image operation")),
    };
    Task::ready(result)
}

pub(super) fn fingerprint(blocks: &[acp::ContentBlock]) -> Option<String> {
    if !blocks
        .iter()
        .any(|block| matches!(block, acp::ContentBlock::Image(_)))
    {
        return None;
    }
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(b"praxis-remote/image-message/v1\0");
    digest.update(text.trim().as_bytes());
    digest.update(b"\0");
    for block in blocks {
        if let acp::ContentBlock::Image(image) = block {
            digest.update(image.mime_type.0.as_bytes());
            digest.update(b"\0");
            digest.update(
                ring::digest::digest(&ring::digest::SHA256, image.data.as_bytes()).as_ref(),
            );
        }
    }
    Some(format!(
        "image-v1:{}",
        crypto::encode(digest.finish().as_ref())
    ))
}

fn validate(bytes: Vec<u8>, mime: &str) -> Result<acp::ContentBlock> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES,
        "The image is empty or too large."
    );
    let mut reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
    let expected = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        _ => bail!("Unsupported image format"),
    };
    ensure!(
        reader.format() == Some(expected),
        "The image contents do not match its format."
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .context("The image is damaged or too large to decode. Choose another image.")?;
    Ok(acp::ContentBlock::Image(acp::ImageContent::new(
        crypto::encode(&bytes),
        mime,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_chunks_are_bounded_ordered_idempotent_and_owner_scoped() {
        let mut uploads = Uploads::default();
        let begin = uploads
            .begin(
                "phone",
                "session",
                &json!({"size": 4, "mime_type": "image/png"}),
            )
            .unwrap();
        let id = begin["upload_id"].as_str().unwrap();
        let chunk = json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[1, 2])});
        assert!(uploads.chunk("other", "session", &chunk).is_err());
        assert!(uploads.chunk("phone", "other", &chunk).is_err());
        assert_eq!(
            uploads.chunk("phone", "session", &chunk).unwrap()["next_offset"],
            2
        );
        assert_eq!(
            uploads.chunk("phone", "session", &chunk).unwrap()["next_offset"],
            2
        );
        assert!(
            uploads
                .chunk(
                    "phone",
                    "session",
                    &json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[3, 4])})
                )
                .is_err()
        );
        let after_gap = uploads
            .chunk(
                "phone",
                "session",
                &json!({"upload_id": id, "offset": 3, "data": crypto::encode(&[3])}),
            )
            .unwrap();
        assert_eq!(after_gap["next_offset"], 2);
        assert_eq!(after_gap["received_bytes"], 3);
        assert!(
            uploads
                .chunk(
                    "phone",
                    "session",
                    &json!({"upload_id": id, "offset": 4, "data": crypto::encode(&[4])})
                )
                .is_err()
        );
        assert!(
            uploads
                .contents("phone", "session", &json!({"images": [id]}))
                .is_err()
        );
        uploads
            .0
            .get_mut(&("phone".into(), id.into()))
            .unwrap()
            .updated = Instant::now() - EXPIRY;
        assert!(uploads.chunk("phone", "session", &chunk).is_err());
    }

    #[test]
    fn image_upload_retries_resume_without_reserving_duplicate_slots() {
        let mut uploads = Uploads::default();
        let args = json!({"client_id": "00000000-0000-4000-8000-000000000001", "size": 4, "mime_type": "image/png"});
        let first = uploads.begin("phone", "session", &args).unwrap();
        let id = first["upload_id"].as_str().unwrap();
        uploads
            .chunk(
                "phone",
                "session",
                &json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[1, 2])}),
            )
            .unwrap();
        let resumed = uploads.begin("phone", "session", &args).unwrap();
        assert_eq!(resumed["upload_id"], id);
        assert_eq!(resumed["next_offset"], 2);
        assert_eq!(uploads.0.len(), 1);
        assert!(uploads.begin("phone", "other", &args).is_err());
        assert_ne!(
            uploads.begin("other", "session", &args).unwrap()["upload_id"],
            id
        );
    }

    #[test]
    fn image_fingerprints_distinguish_image_only_messages_and_captions() {
        let one = acp::ContentBlock::Image(acp::ImageContent::new("one", "image/png"));
        let two = acp::ContentBlock::Image(acp::ImageContent::new("two", "image/png"));
        assert_ne!(fingerprint(std::slice::from_ref(&one)), fingerprint(&[two]));
        assert_ne!(
            fingerprint(std::slice::from_ref(&one)),
            fingerprint(&[
                acp::ContentBlock::Text(acp::TextContent::new("caption")),
                one
            ])
        );
        assert!(fingerprint(&[acp::ContentBlock::Text(acp::TextContent::new("text"))]).is_none());
    }

    #[test]
    fn image_validation_rejects_malformed_mismatched_and_oversized_uploads() {
        assert!(validate(vec![1, 2, 3], "image/png").is_err());
        let png = crypto::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==").unwrap();
        assert!(validate(png.clone(), "image/jpeg").is_err());
        assert!(matches!(
            validate(png, "image/png").unwrap(),
            acp::ContentBlock::Image(_)
        ));
        assert!(
            Uploads::default()
                .begin(
                    "phone",
                    "session",
                    &json!({"size": MAX_IMAGE_BYTES + 1, "mime_type": "image/png"})
                )
                .is_err()
        );
    }

    #[test]
    fn out_of_order_image_chunks_assemble_and_finish_cleanly() {
        let png = crypto::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==").unwrap();
        let mut uploads = Uploads::default();
        let begin = uploads
            .begin(
                "phone",
                "session",
                &json!({
                    "size": png.len(),
                    "mime_type": "image/png",
                    "preferred_chunk_bytes": 45056,
                }),
            )
            .unwrap();
        assert_eq!(begin["chunk_bytes"], MAX_CHUNK_BYTES);
        assert_eq!(begin["max_parallel"], MAX_PARALLEL_CHUNKS);
        let id = begin["upload_id"].as_str().unwrap();

        let third = png.len() / 3;
        let two_thirds = third * 2;

        let c3 = uploads
            .chunk(
                "phone",
                "session",
                &json!({
                    "upload_id": id,
                    "offset": two_thirds,
                    "data": crypto::encode(&png[two_thirds..]),
                }),
            )
            .unwrap();
        assert_eq!(c3["next_offset"], 0);
        assert_eq!(c3["received_bytes"], png.len() - two_thirds);

        let c2 = uploads
            .chunk(
                "phone",
                "session",
                &json!({
                    "upload_id": id,
                    "offset": third,
                    "data": crypto::encode(&png[third..two_thirds]),
                }),
            )
            .unwrap();
        assert_eq!(c2["next_offset"], 0);
        assert_eq!(c2["received_bytes"], png.len() - third);

        let c1 = uploads
            .chunk(
                "phone",
                "session",
                &json!({
                    "upload_id": id,
                    "offset": 0,
                    "data": crypto::encode(&png[..third]),
                }),
            )
            .unwrap();
        assert_eq!(c1["next_offset"], png.len());
        assert_eq!(c1["received_bytes"], png.len());

        let upload = uploads.get("phone", "session", id).unwrap();
        assert!(upload.buffer.is_complete());
        let assembled = upload.buffer.take_bytes().unwrap();
        assert_eq!(assembled, png);
        assert!(matches!(
            validate(assembled, "image/png").unwrap(),
            acp::ContentBlock::Image(_)
        ));
    }
}
