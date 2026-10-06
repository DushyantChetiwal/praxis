use std::collections::HashMap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, bail, ensure};
use gpui::{App, AppContext as _, Global, Task};
use image::{ImageFormat, ImageReader};
use serde_json::{Value, json};

use super::{crypto, required, transcript};

const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 24 * 1024;
const MAX_IMAGES: usize = 4;
const MAX_BUFFERED_BYTES: usize = 16 * 1024 * 1024;
const EXPIRY: Duration = Duration::from_secs(300);

type UploadKey = (String, String);

struct Upload {
    session: String,
    mime: String,
    size: usize,
    bytes: Vec<u8>,
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
        ensure!((1..=MAX_IMAGE_BYTES).contains(&size), "Images must be at most 2 MiB. Choose a smaller image.");
        let mime = required(args, "mime_type")?;
        ensure!(matches!(mime, "image/png" | "image/jpeg" | "image/webp"), "Choose a PNG, JPEG, or WebP image.");
        ensure!(self.0.keys().filter(|(owner, _)| owner == phone).count() < MAX_IMAGES, "Finish or remove an image upload before adding another.");
        let reserved: usize = self.0.values().map(|upload| upload.size).sum();
        ensure!(reserved.saturating_add(size) <= MAX_BUFFERED_BYTES, "Praxis has too many pending image uploads. Retry shortly.");
        let id = crypto::random_id()?;
        self.0.insert((phone.into(), id.clone()), Upload {
            session: session.into(), mime: mime.into(), size,
            bytes: Vec::new(), content: None, updated: Instant::now(),
        });
        Ok(json!({ "upload_id": id, "chunk_bytes": MAX_CHUNK_BYTES }))
    }

    fn get(&mut self, phone: &str, session: &str, id: &str) -> Result<&mut Upload> {
        self.prune();
        let upload = self.0.get_mut(&(phone.into(), id.into()))
            .context("That image upload expired or belongs to another phone. Attach it again.")?;
        ensure!(upload.session == session, "That image belongs to another conversation. Attach it again here.");
        Ok(upload)
    }

    fn chunk(&mut self, phone: &str, session: &str, args: &Value) -> Result<Value> {
        let id = required(args, "upload_id")?;
        let offset = transcript::index(args, "offset")?.context("expected image offset")?;
        let encoded = required(args, "data")?;
        ensure!(encoded.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4, "The image chunk is too large.");
        let bytes = crypto::decode(encoded)?;
        ensure!(!bytes.is_empty() && bytes.len() <= MAX_CHUNK_BYTES, "The image chunk is empty or too large.");
        let upload = self.get(phone, session, id)?;
        ensure!(upload.content.is_none(), "That image upload is already complete.");
        let end = offset.checked_add(bytes.len()).context("invalid image offset")?;
        ensure!(end <= upload.size, "The image chunk exceeds the announced image size.");
        if offset < upload.bytes.len() {
            ensure!(upload.bytes.get(offset..end) == Some(bytes.as_slice()), "The image chunk conflicts with an earlier chunk.");
        } else {
            ensure!(offset == upload.bytes.len(), "An image chunk is missing. Restart the upload.");
            upload.bytes.extend_from_slice(&bytes);
        }
        upload.updated = Instant::now();
        Ok(json!({ "next_offset": upload.bytes.len() }))
    }

    fn contents(&mut self, phone: &str, session: &str, args: &Value) -> Result<Vec<acp::ContentBlock>> {
        let Some(value) = args.get("images") else { return Ok(Vec::new()); };
        let ids = value.as_array().context("expected image upload IDs")?;
        ensure!(!ids.is_empty() && ids.len() <= MAX_IMAGES, "Attach between one and four images.");
        let mut used = std::collections::HashSet::new();
        let mut content = Vec::new();
        for value in ids {
            let id = value.as_str().context("expected an image upload ID")?;
            ensure!(used.insert(id), "An image was attached more than once.");
            content.push(self.get(phone, session, id)?.content.clone().context("An image is still uploading. Wait for it to finish.")?);
        }
        Ok(content)
    }
}

fn uploads(cx: &mut App) -> &mut Uploads {
    if !cx.has_global::<Uploads>() { cx.set_global(Uploads::default()); }
    cx.global_mut::<Uploads>()
}

pub(super) fn clear(phone: &str, cx: &mut App) {
    if cx.has_global::<Uploads>() {
        cx.global_mut::<Uploads>().0.retain(|(owner, _), _| owner != phone);
    }
}

pub(super) fn contents(phone: &str, session: &str, args: &Value, cx: &mut App) -> Result<Vec<acp::ContentBlock>> {
    uploads(cx).contents(phone, session, args)
}

pub(super) fn consume(phone: &str, args: &Value, cx: &mut App) {
    if let Some(ids) = args.get("images").and_then(Value::as_array) {
        for id in ids.iter().filter_map(Value::as_str) {
            uploads(cx).0.remove(&(phone.into(), id.into()));
        }
    }
}

pub(super) fn handle(phone: &str, session: &str, op: &str, args: &Value, cx: &mut App) -> Task<Result<Value>> {
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
                ensure!(upload.bytes.len() == upload.size, "The image upload is incomplete.");
                Ok((id, upload.bytes.clone(), upload.mime.clone()))
            })();
            let (id, bytes, mime) = match pending {
                Ok(pending) => pending,
                Err(error) => return Task::ready(Err(error)),
            };
            let phone = phone.to_string();
            let session = session.to_string();
            return cx.spawn(async move |cx| {
                let content = cx.background_spawn(async move { validate(bytes, &mime) }).await?;
                cx.update(|cx| {
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


fn validate(bytes: Vec<u8>, mime: &str) -> Result<acp::ContentBlock> {
    ensure!(!bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES, "The image is empty or too large.");
    let mut reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
    let expected = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/webp" => ImageFormat::WebP,
        _ => bail!("Unsupported image format"),
    };
    ensure!(reader.format() == Some(expected), "The image contents do not match its format.");
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().context("The image is damaged or too large to decode. Choose another image.")?;
    Ok(acp::ContentBlock::Image(acp::ImageContent::new(crypto::encode(&bytes), mime)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_chunks_are_bounded_ordered_idempotent_and_owner_scoped() {
        let mut uploads = Uploads::default();
        let begin = uploads.begin("phone", "session", &json!({"size": 4, "mime_type": "image/png"})).unwrap();
        let id = begin["upload_id"].as_str().unwrap();
        let chunk = json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[1, 2])});
        assert!(uploads.chunk("other", "session", &chunk).is_err());
        assert!(uploads.chunk("phone", "other", &chunk).is_err());
        assert_eq!(uploads.chunk("phone", "session", &chunk).unwrap()["next_offset"], 2);
        assert_eq!(uploads.chunk("phone", "session", &chunk).unwrap()["next_offset"], 2);
        assert!(uploads.chunk("phone", "session", &json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[3, 4])})).is_err());
        assert!(uploads.chunk("phone", "session", &json!({"upload_id": id, "offset": 3, "data": crypto::encode(&[3])})).is_err());
        assert!(uploads.contents("phone", "session", &json!({"images": [id]})).is_err());
        uploads.0.get_mut(&("phone".into(), id.into())).unwrap().updated = Instant::now() - EXPIRY;
        assert!(uploads.chunk("phone", "session", &chunk).is_err());
    }

    #[test]
    fn image_validation_rejects_malformed_mismatched_and_oversized_uploads() {
        assert!(validate(vec![1, 2, 3], "image/png").is_err());
        let png = crypto::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==").unwrap();
        assert!(validate(png.clone(), "image/jpeg").is_err());
        assert!(matches!(validate(png, "image/png").unwrap(), acp::ContentBlock::Image(_)));
        assert!(Uploads::default().begin("phone", "session", &json!({"size": MAX_IMAGE_BYTES + 1, "mime_type": "image/png"})).is_err());
    }
}
