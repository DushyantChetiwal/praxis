//! Files a paired phone uploads into a project, either into the folder shown
//! in its Files tab or as attachments to a chat message.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, ensure};
use gpui::{App, Global, Task};
use serde_json::{Value, json};

use super::{crypto, ensure_inside, required, resolve_project_path, transcript};

pub(super) const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
const MAX_CHUNK_BYTES: usize = 24 * 1024;
const MAX_PENDING_UPLOADS: usize = 4;
const MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;
const MAX_ATTACHED_FILES: usize = 8;
const MAX_NAME_CHARS: usize = 120;
const EXPIRY: Duration = Duration::from_secs(600);
/// Where chat attachments go, relative to the first project folder.
const ATTACHMENTS_DIR: &str = ".praxis/uploads";

type UploadKey = (String, String);

struct Upload {
    client_id: String,
    directory: PathBuf,
    display_directory: String,
    attachment: bool,
    name: String,
    size: usize,
    bytes: Vec<u8>,
    saved_path: Option<String>,
    updated: Instant,
}

#[derive(Default)]
struct Uploads(HashMap<UploadKey, Upload>);
impl Global for Uploads {}

/// The folder an upload is saved into, worked out on the main thread where
/// the project is known.
pub(super) struct Destination {
    directory: PathBuf,
    display_directory: String,
    attachment: bool,
}

/// Where `file_begin` saves: the folder the phone is browsing, or the first
/// project folder's attachment folder for a chat message.
pub(super) fn destination(roots: &[(String, PathBuf)], args: &Value) -> Result<Destination> {
    if args.get("attachment").and_then(Value::as_bool) == Some(true) {
        let (root_name, root) = roots
            .first()
            .context("Open a project folder before attaching files.")?;
        return Ok(Destination {
            directory: root.join(ATTACHMENTS_DIR),
            display_directory: format!("{root_name}/{ATTACHMENTS_DIR}"),
            attachment: true,
        });
    }
    let (display, path) = resolve_project_path(roots, required(args, "path")?)?;
    let directory = ensure_inside(roots, &path)?;
    ensure!(directory.is_dir(), "{display} is not a folder");
    Ok(Destination {
        directory,
        display_directory: display,
        attachment: false,
    })
}

/// A file name that is safe on every desktop: the last path component, with
/// characters Windows refuses removed and its reserved device names avoided.
pub(super) fn safe_file_name(name: &str) -> Result<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut cleaned: String = last
        .chars()
        .filter(|character| {
            !character.is_control() && !matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
        .collect();
    cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    ensure!(
        !cleaned.is_empty() && cleaned != "." && cleaned != "..",
        "The file needs a name."
    );
    if cleaned.chars().count() > MAX_NAME_CHARS {
        let (stem, extension) = split_extension(&cleaned);
        let keep = MAX_NAME_CHARS.saturating_sub(extension.chars().count());
        cleaned = stem.chars().take(keep).collect::<String>() + extension;
    }
    let (stem, _) = split_extension(&cleaned);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
    ) || {
        let upper = stem.to_ascii_uppercase();
        (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper
                .chars()
                .nth(3)
                .is_some_and(|digit| digit.is_ascii_digit())
    };
    if reserved {
        cleaned.insert(0, '_');
    }
    Ok(cleaned)
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => name.split_at(index),
        _ => (name, ""),
    }
}

/// Writes the file under the first free name, so nothing already in the
/// project is overwritten.
fn save(directory: &Path, name: &str, bytes: &[u8], attachment: bool) -> Result<String> {
    if attachment {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        // Uploads are the user's files, not the project's.
        let ignore = directory.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*\n").context("writing the uploads .gitignore")?;
        }
    }
    let (stem, extension) = split_extension(name);
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){extension}")
        };
        let path = directory.join(&candidate);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            if let Err(cleanup) = std::fs::remove_file(&path) {
                log::warn!("Could not remove a partial upload: {cleanup}");
            }
            return Err(error).with_context(|| format!("writing {}", path.display()));
        }
        return Ok(candidate);
    }
    anyhow::bail!("Too many files named {name} already exist here. Rename the file and retry.")
}

impl Uploads {
    fn prune(&mut self) {
        self.0.retain(|_, upload| upload.updated.elapsed() < EXPIRY);
    }

    fn begin(&mut self, phone: &str, destination: Destination, args: &Value) -> Result<Value> {
        self.prune();
        let size = transcript::index(args, "size")?.context("expected the file size")?;
        ensure!(
            (1..=MAX_UPLOAD_BYTES).contains(&size),
            "Files must be at most 25 MB. Choose a smaller file."
        );
        let name = safe_file_name(required(args, "name")?)?;
        let client_id = args.get("client_id").and_then(Value::as_str).unwrap_or("");
        if !client_id.is_empty() {
            uuid::Uuid::parse_str(client_id).context("expected a file client ID")?;
            if let Some(((_, id), upload)) = self
                .0
                .iter()
                .find(|((owner, _), upload)| owner == phone && upload.client_id == client_id)
            {
                ensure!(
                    upload.directory == destination.directory
                        && upload.name == name
                        && upload.size == size,
                    "A file ID was reused with different upload details."
                );
                return Ok(json!({
                    "upload_id": id,
                    "chunk_bytes": MAX_CHUNK_BYTES,
                    "next_offset": upload.bytes.len(),
                    "path": upload.saved_path,
                }));
            }
        }
        ensure!(
            self.0.keys().filter(|(owner, _)| owner == phone).count() < MAX_PENDING_UPLOADS,
            "Finish or cancel an upload before starting another."
        );
        let reserved: usize = self.0.values().map(|upload| upload.size).sum();
        ensure!(
            reserved.saturating_add(size) <= MAX_BUFFERED_BYTES,
            "Praxis has too many pending uploads. Retry shortly."
        );
        let id = crypto::random_id()?;
        self.0.insert(
            (phone.into(), id.clone()),
            Upload {
                client_id: client_id.into(),
                directory: destination.directory,
                display_directory: destination.display_directory,
                attachment: destination.attachment,
                name,
                size,
                bytes: Vec::new(),
                saved_path: None,
                updated: Instant::now(),
            },
        );
        Ok(json!({ "upload_id": id, "chunk_bytes": MAX_CHUNK_BYTES, "next_offset": 0 }))
    }

    fn get(&mut self, phone: &str, id: &str) -> Result<&mut Upload> {
        self.prune();
        self.0
            .get_mut(&(phone.into(), id.into()))
            .context("That upload expired or belongs to another phone. Upload the file again.")
    }

    fn chunk(&mut self, phone: &str, args: &Value) -> Result<Value> {
        let id = required(args, "upload_id")?;
        let offset = transcript::index(args, "offset")?.context("expected the chunk offset")?;
        let encoded = required(args, "data")?;
        ensure!(
            encoded.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4,
            "The file chunk is too large."
        );
        let bytes = crypto::decode(encoded)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CHUNK_BYTES,
            "The file chunk is empty or too large."
        );
        let upload = self.get(phone, id)?;
        ensure!(upload.saved_path.is_none(), "That upload is already saved.");
        let end = offset
            .checked_add(bytes.len())
            .context("invalid chunk offset")?;
        ensure!(
            end <= upload.size,
            "The chunk goes past the announced file size."
        );
        if offset < upload.bytes.len() {
            ensure!(
                upload.bytes.get(offset..end) == Some(bytes.as_slice()),
                "The chunk conflicts with an earlier one."
            );
        } else {
            ensure!(
                offset == upload.bytes.len(),
                "A chunk is missing. Restart the upload."
            );
            upload.bytes.extend_from_slice(&bytes);
        }
        upload.updated = Instant::now();
        Ok(json!({ "next_offset": upload.bytes.len() }))
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

pub(super) fn begin(
    phone: &str,
    destination: Destination,
    args: &Value,
    cx: &mut App,
) -> Result<Value> {
    uploads(cx).begin(phone, destination, args)
}

pub(super) fn handle(phone: &str, op: &str, args: &Value, cx: &mut App) -> Task<Result<Value>> {
    let result = match op {
        "file_chunk" => uploads(cx).chunk(phone, args),
        "file_discard" => discard(phone, args, cx),
        "file_finish" => return finish(phone, args, cx),
        _ => Err(anyhow::anyhow!("Unknown upload operation")),
    };
    Task::ready(result)
}

fn discard(phone: &str, args: &Value, cx: &mut App) -> Result<Value> {
    let id = required(args, "upload_id")?;
    uploads(cx).get(phone, id)?;
    uploads(cx).0.remove(&(phone.into(), id.into()));
    Ok(json!({ "discarded": true }))
}

fn finish(phone: &str, args: &Value, cx: &mut App) -> Task<Result<Value>> {
    let id = match required(args, "upload_id") {
        Ok(id) => id.to_string(),
        Err(error) => return Task::ready(Err(error)),
    };
    let upload = match uploads(cx).get(phone, &id) {
        Ok(upload) => upload,
        Err(error) => return Task::ready(Err(error)),
    };
    // A retried finish gets the answer the first one gave.
    if let Some(path) = &upload.saved_path {
        return Task::ready(Ok(json!({ "upload_id": id, "path": path })));
    }
    if upload.bytes.len() != upload.size {
        return Task::ready(Err(anyhow::anyhow!("The upload is incomplete.")));
    }
    let directory = upload.directory.clone();
    let display_directory = upload.display_directory.clone();
    let name = upload.name.clone();
    let attachment = upload.attachment;
    let bytes = std::mem::take(&mut upload.bytes);
    let phone = phone.to_string();
    cx.spawn(async move |cx| {
        let saved = cx
            .background_spawn(async move { save(&directory, &name, &bytes, attachment) })
            .await;
        cx.update(|cx| {
            let saved = match saved {
                Ok(saved) => saved,
                Err(error) => {
                    // Nothing was kept, so the phone starts the upload over.
                    uploads(cx).0.remove(&(phone.clone(), id.clone()));
                    return Err(error);
                }
            };
            let path = format!("{display_directory}/{saved}");
            let upload = uploads(cx).get(&phone, &id)?;
            upload.saved_path = Some(path.clone());
            upload.updated = Instant::now();
            Ok(json!({ "upload_id": id, "path": path }))
        })
    })
}

/// The files a chat message attaches, as links the agent can read.
pub(super) fn attachments(
    roots: &[(String, PathBuf)],
    args: &Value,
) -> Result<Vec<acp::ContentBlock>> {
    let Some(value) = args.get("files") else {
        return Ok(Vec::new());
    };
    let paths = value.as_array().context("expected attached file paths")?;
    ensure!(
        !paths.is_empty() && paths.len() <= MAX_ATTACHED_FILES,
        "Attach between one and eight files."
    );
    paths
        .iter()
        .map(|path| {
            let path = path.as_str().context("expected an attached file path")?;
            let (display, resolved) = resolve_project_path(roots, path)?;
            ensure!(
                ensure_inside(roots, &resolved)?.is_file(),
                "{display} is not a file"
            );
            // The path as the project names it, not its canonical form, which
            // on Windows carries a `\\?\` prefix the agent would not recognize.
            let mention = acp_thread::MentionUri::File { abs_path: resolved };
            Ok(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                mention.name(),
                mention.to_uri(),
            )))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe_on_every_desktop() {
        assert_eq!(safe_file_name("photo.jpg").unwrap(), "photo.jpg");
        assert_eq!(safe_file_name("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(
            safe_file_name("C:\\Users\\me\\report.pdf").unwrap(),
            "report.pdf"
        );
        assert_eq!(safe_file_name("a<b>c:d|e?f*.txt").unwrap(), "abcdef.txt");
        assert_eq!(safe_file_name("notes. ").unwrap(), "notes");
        assert_eq!(safe_file_name("CON.txt").unwrap(), "_CON.txt");
        assert_eq!(safe_file_name("com3").unwrap(), "_com3");
        assert_eq!(safe_file_name("computer.txt").unwrap(), "computer.txt");
        assert!(safe_file_name("..").is_err());
        assert!(safe_file_name("folder/").is_err());
        let long = format!("{}.tar.gz", "x".repeat(300));
        let shortened = safe_file_name(&long).unwrap();
        assert_eq!(shortened.chars().count(), MAX_NAME_CHARS);
        assert!(shortened.ends_with(".gz"));
    }

    #[test]
    fn saving_never_overwrites_and_ignores_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let uploads = directory.path().join(".praxis").join("uploads");
        assert_eq!(save(&uploads, "a.txt", b"one", true).unwrap(), "a.txt");
        assert_eq!(save(&uploads, "a.txt", b"two", true).unwrap(), "a (1).txt");
        assert_eq!(std::fs::read(uploads.join("a.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(uploads.join("a (1).txt")).unwrap(), b"two");
        assert_eq!(
            std::fs::read_to_string(uploads.join(".gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(
            save(directory.path(), "README", b"x", false).unwrap(),
            "README"
        );
        assert!(!directory.path().join(".gitignore").exists());
    }

    #[test]
    fn chunks_are_ordered_idempotent_and_owner_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let mut uploads = Uploads::default();
        let destination = || Destination {
            directory: directory.path().to_path_buf(),
            display_directory: "project".into(),
            attachment: false,
        };
        let args = json!({
            "client_id": "00000000-0000-4000-8000-000000000001",
            "name": "notes.txt",
            "size": 4,
        });
        let begin = uploads.begin("phone", destination(), &args).unwrap();
        let id = begin["upload_id"].as_str().unwrap().to_string();
        let chunk = json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[1, 2])});
        assert!(uploads.chunk("other", &chunk).is_err());
        assert_eq!(uploads.chunk("phone", &chunk).unwrap()["next_offset"], 2);
        assert_eq!(uploads.chunk("phone", &chunk).unwrap()["next_offset"], 2);
        let gap = json!({"upload_id": id, "offset": 3, "data": crypto::encode(&[4])});
        assert!(uploads.chunk("phone", &gap).is_err());
        let resumed = uploads.begin("phone", destination(), &args).unwrap();
        assert_eq!(resumed["upload_id"], id.as_str());
        assert_eq!(resumed["next_offset"], 2);
        assert_eq!(uploads.0.len(), 1);
        let too_large = json!({"name": "big.bin", "size": MAX_UPLOAD_BYTES + 1});
        assert!(uploads.begin("phone", destination(), &too_large).is_err());
    }

    #[test]
    fn attachments_stay_inside_the_project() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        std::fs::create_dir_all(root.join(".praxis/uploads")).unwrap();
        std::fs::write(root.join(".praxis/uploads/a.txt"), "a").unwrap();
        std::fs::write(directory.path().join("outside.txt"), "x").unwrap();
        let roots = vec![("project".to_string(), root.clone())];
        let links =
            attachments(&roots, &json!({"files": ["project/.praxis/uploads/a.txt"]})).unwrap();
        let [acp::ContentBlock::ResourceLink(link)] = links.as_slice() else {
            panic!("expected one link, got {links:?}");
        };
        assert_eq!(link.name, "a.txt");
        assert!(attachments(&roots, &json!({"files": ["project/../outside.txt"]})).is_err());
        assert!(attachments(&roots, &json!({"files": ["project/.praxis"]})).is_err());
        assert!(attachments(&roots, &json!({"files": []})).is_err());
        assert!(attachments(&roots, &json!({})).unwrap().is_empty());
    }
}
