//! Files a paired phone uploads into a project, either into the folder shown
//! in its Files tab or as attachments to a chat message.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result, ensure};
use fs::Fs;
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, Task};
use project::{Project, Worktree};
use serde_json::{Value, json};
use util::paths::PathStyle;
use util::rel_path::RelPath;

use super::{
    crypto, ensure_inside, required, resolve_project_path, split_project_path, transcript,
};

pub(super) const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
pub(super) const DEFAULT_CHUNK_BYTES: usize = 24 * 1024;
pub(super) const MAX_CHUNK_BYTES: usize = 44 * 1024;
pub(super) const MAX_PARALLEL_CHUNKS: usize = 6;
const MAX_PENDING_UPLOADS: usize = 4;
const MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;
const MAX_ATTACHED_FILES: usize = 8;
const MAX_NAME_CHARS: usize = 120;
const EXPIRY: Duration = Duration::from_secs(600);
/// Where chat attachments go, relative to the first project folder.
const ATTACHMENTS_DIR: &str = ".praxis/uploads";

type UploadKey = (String, String);

pub(super) fn negotiate_chunk_bytes(args: &Value) -> Result<usize> {
    Ok(match transcript::index(args, "preferred_chunk_bytes")? {
        Some(preferred) if preferred > 0 => preferred.min(MAX_CHUNK_BYTES),
        _ => DEFAULT_CHUNK_BYTES,
    })
}

/// Collects out-of-order byte chunks into minimal disjoint contiguous ranges.
pub(super) struct ChunkBuffer {
    size: usize,
    chunks: BTreeMap<usize, Vec<u8>>,
    contiguous_offset: usize,
    received_bytes: usize,
}

impl ChunkBuffer {
    pub(super) fn new(size: usize) -> Self {
        Self {
            size,
            chunks: BTreeMap::new(),
            contiguous_offset: 0,
            received_bytes: 0,
        }
    }

    pub(super) fn contiguous_offset(&self) -> usize {
        self.contiguous_offset
    }

    pub(super) fn received_bytes(&self) -> usize {
        self.received_bytes
    }

    pub(super) fn is_complete(&self) -> bool {
        self.contiguous_offset == self.size && self.received_bytes == self.size
    }

    pub(super) fn insert(&mut self, offset: usize, bytes: Vec<u8>) -> Result<()> {
        ensure!(!bytes.is_empty(), "The chunk is empty.");
        let end = offset
            .checked_add(bytes.len())
            .context("invalid chunk offset")?;
        ensure!(end <= self.size, "The chunk goes past the announced size.");

        if let Some((&existing_start, existing)) = self.chunks.range(..offset).next_back() {
            let existing_end = existing_start + existing.len();
            if existing_end > offset {
                let overlap_end = existing_end.min(end);
                ensure!(
                    existing[(offset - existing_start)..(overlap_end - existing_start)]
                        == bytes[..(overlap_end - offset)],
                    "The chunk conflicts with an earlier one."
                );
            }
        }
        for (&existing_start, existing) in self.chunks.range(offset..end) {
            let overlap_end = (existing_start + existing.len()).min(end);
            ensure!(
                existing[..(overlap_end - existing_start)]
                    == bytes[(existing_start - offset)..(overlap_end - offset)],
                "The chunk conflicts with an earlier one."
            );
        }

        let left_key =
            self.chunks
                .range(..offset)
                .next_back()
                .and_then(|(&existing_start, existing)| {
                    (existing_start + existing.len() >= offset).then_some(existing_start)
                });

        let (merged_start, mut merged) = if let Some(left_start) = left_key {
            let mut left = self
                .chunks
                .remove(&left_start)
                .context("missing overlapping chunk")?;
            self.received_bytes -= left.len();
            let left_end = left_start + left.len();
            if end > left_end {
                left.extend_from_slice(&bytes[(left_end - offset)..]);
            }
            (left_start, left)
        } else {
            (offset, bytes)
        };

        let merged_end = merged_start + merged.len();
        let right_keys: Vec<usize> = self
            .chunks
            .range(offset..=merged_end)
            .map(|(&existing_start, _)| existing_start)
            .collect();
        for key in right_keys {
            if let Some(existing) = self.chunks.remove(&key) {
                self.received_bytes -= existing.len();
                let existing_end = key + existing.len();
                let current_end = merged_start + merged.len();
                if existing_end > current_end {
                    merged.extend_from_slice(&existing[(current_end - key)..]);
                }
            }
        }

        self.received_bytes += merged.len();
        self.chunks.insert(merged_start, merged);
        self.contiguous_offset = self.chunks.get(&0).map_or(0, Vec::len);
        Ok(())
    }

    pub(super) fn take_bytes(&mut self) -> Result<Vec<u8>> {
        ensure!(self.is_complete(), "The upload is incomplete.");
        if self.size == 0 {
            return Ok(Vec::new());
        }
        let bytes = self
            .chunks
            .remove(&0)
            .context("The upload is incomplete.")?;
        self.contiguous_offset = 0;
        self.received_bytes = 0;
        Ok(bytes)
    }
}

struct Upload {
    client_id: String,
    target: UploadTarget,
    directory: PathBuf,
    display_directory: String,
    attachment: bool,
    name: String,
    size: usize,
    chunk_bytes: usize,
    buffer: ChunkBuffer,
    saved_path: Option<String>,
    updated: Instant,
}

#[derive(Default)]
struct Uploads(HashMap<UploadKey, Upload>);
impl Global for Uploads {}

/// Where an upload is written: this computer's filesystem, or a folder inside
/// a worktree on the computer the project lives on.
#[derive(Clone)]
pub(super) enum UploadTarget {
    Local(Arc<dyn Fs>),
    Remote {
        project: Entity<Project>,
        worktree: Entity<Worktree>,
        directory: Arc<RelPath>,
    },
}

/// The folder an upload is saved into, worked out on the main thread where
/// the project is known.
pub(super) struct Destination {
    target: UploadTarget,
    directory: PathBuf,
    display_directory: String,
    attachment: bool,
}

/// Where `file_begin` saves: the folder the phone is browsing, or the first
/// project folder's attachment folder for a chat message.
pub(super) fn destination(
    project: Entity<Project>,
    roots: &[(String, PathBuf)],
    args: &Value,
    cx: &App,
) -> Result<Destination> {
    let attachment = args.get("attachment").and_then(Value::as_bool) == Some(true);
    if project.read(cx).remote_connection_options(cx).is_some() {
        return remote_destination(&project, roots, args, attachment, cx);
    }
    let (directory, display_directory) = if attachment {
        let (root_name, root) = roots
            .first()
            .context("Open a project folder before attaching files.")?;
        (
            root.join(ATTACHMENTS_DIR),
            format!("{root_name}/{ATTACHMENTS_DIR}"),
        )
    } else {
        let (display, path) = resolve_project_path(roots, required(args, "path")?)?;
        (ensure_inside(roots, &path)?, display)
    };
    Ok(Destination {
        target: UploadTarget::Local(project.read(cx).fs().clone()),
        directory,
        display_directory,
        attachment,
    })
}

/// A project on WSL or another computer is addressed by worktree and relative
/// path: this computer can neither resolve that computer's links nor apply its
/// path rules, and the remote server keeps the write inside the worktree.
fn remote_destination(
    project: &Entity<Project>,
    roots: &[(String, PathBuf)],
    args: &Value,
    attachment: bool,
    cx: &App,
) -> Result<Destination> {
    let (root_name, relative) = if attachment {
        let (root_name, _) = roots
            .first()
            .context("Open a project folder before attaching files.")?;
        (root_name.as_str(), ATTACHMENTS_DIR)
    } else {
        split_project_path(roots, required(args, "path")?)?
    };
    let worktree = project
        .read(cx)
        .visible_worktrees(cx)
        .find(|worktree| worktree.read(cx).root_name_str() == root_name)
        .context("That folder is not inside a project folder.")?;
    let directory = RelPath::new(Path::new(relative), PathStyle::Unix)?.into_arc();
    let display_directory = if relative.is_empty() {
        root_name.to_string()
    } else {
        format!("{root_name}/{}", directory.as_unix_str())
    };
    Ok(Destination {
        target: UploadTarget::Remote {
            project: project.clone(),
            worktree,
            directory,
        },
        directory: PathBuf::from(&display_directory),
        display_directory,
        attachment,
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
async fn save_local(
    fs: &dyn Fs,
    directory: &Path,
    name: &str,
    bytes: &[u8],
    attachment: bool,
) -> Result<String> {
    if attachment {
        fs.create_dir(directory)
            .await
            .with_context(|| format!("creating {}", directory.display()))?;
        // Uploads are the user's files, not the project's.
        let ignore = directory.join(".gitignore");
        if fs.metadata(&ignore).await?.is_none() {
            fs.write(&ignore, b"*\n")
                .await
                .context("writing the uploads .gitignore")?;
        }
    }
    let metadata = fs.metadata(directory).await?;
    ensure!(
        metadata.is_some_and(|metadata| metadata.is_dir),
        "{} is not a folder",
        directory.display()
    );
    let (stem, extension) = split_extension(name);
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){extension}")
        };
        let path = directory.join(&candidate);
        if fs.metadata(&path).await?.is_some() {
            continue;
        }
        if let Err(error) = fs.write(&path, bytes).await {
            if let Err(cleanup) = fs
                .remove_file(
                    &path,
                    ::fs::RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await
            {
                log::warn!("Could not remove a partial upload: {cleanup}");
            }
            return Err(error).with_context(|| format!("writing {}", path.display()));
        }
        return Ok(candidate);
    }
    anyhow::bail!("Too many files named {name} already exist here. Rename the file and retry.")
}

/// Whether a path exists on the computer the project lives on. The worktree's
/// snapshot cannot answer this: it does not load ignored folders such as the
/// attachment folder, and creating an entry there overwrites silently.
async fn exists_remotely(
    project: &Entity<Project>,
    worktree: &Entity<Worktree>,
    path: &RelPath,
    cx: &mut AsyncApp,
) -> bool {
    let abs_path = worktree.read_with(cx, |worktree, _| worktree.absolutize(path));
    let abs_path = abs_path.to_string_lossy();
    project
        .read_with(cx, |project, cx| project.resolve_abs_path(&abs_path, cx))
        .await
        .is_some()
}

/// Writes the file into a worktree on the computer the project lives on, which
/// is the only way to save an upload there.
async fn save_remote(
    project: Entity<Project>,
    worktree: Entity<Worktree>,
    directory: &RelPath,
    name: &str,
    bytes: &[u8],
    attachment: bool,
    cx: &mut AsyncApp,
) -> Result<String> {
    if attachment {
        if !exists_remotely(&project, &worktree, directory, cx).await {
            for folder in directory.ancestors().collect::<Vec<_>>().into_iter().rev() {
                if folder.is_empty() || exists_remotely(&project, &worktree, folder, cx).await {
                    continue;
                }
                if let Err(error) = worktree
                    .update(cx, |worktree, cx| {
                        worktree.create_entry(folder.into_arc(), true, None, cx)
                    })
                    .await
                    && !exists_remotely(&project, &worktree, folder, cx).await
                {
                    return Err(error);
                }
            }
        }
        // Uploads are the user's files, not the project's.
        let ignore = directory.join(RelPath::from_unix_str(".gitignore")?);
        if !exists_remotely(&project, &worktree, &ignore, cx).await
            && let Err(error) = worktree
                .update(cx, |worktree, cx| {
                    worktree.create_entry(ignore.into_arc(), false, Some(b"*\n".to_vec()), cx)
                })
                .await
            && !exists_remotely(&project, &worktree, &ignore, cx).await
        {
            return Err(error);
        }
    }

    let (stem, extension) = split_extension(name);
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){extension}")
        };
        let path = directory.join(&RelPath::new(Path::new(&candidate), PathStyle::Unix)?);
        if exists_remotely(&project, &worktree, &path, cx).await {
            continue;
        }
        worktree
            .update(cx, |worktree, cx| {
                worktree.create_entry(path.into_arc(), false, Some(bytes.to_vec()), cx)
            })
            .await?;
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
        let chunk_bytes = negotiate_chunk_bytes(args)?;
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
                    "chunk_bytes": upload.chunk_bytes,
                    "max_parallel": MAX_PARALLEL_CHUNKS,
                    "next_offset": upload.buffer.contiguous_offset(),
                    "received_bytes": upload.buffer.received_bytes(),
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
                target: destination.target.clone(),
                directory: destination.directory,
                display_directory: destination.display_directory,
                attachment: destination.attachment,
                name,
                size,
                chunk_bytes,
                buffer: ChunkBuffer::new(size),
                saved_path: None,
                updated: Instant::now(),
            },
        );
        Ok(json!({
            "upload_id": id,
            "chunk_bytes": chunk_bytes,
            "max_parallel": MAX_PARALLEL_CHUNKS,
            "next_offset": 0,
            "received_bytes": 0,
        }))
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
        upload.buffer.insert(offset, bytes)?;
        upload.updated = Instant::now();
        Ok(json!({
            "next_offset": upload.buffer.contiguous_offset(),
            "received_bytes": upload.buffer.received_bytes(),
        }))
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
    if !upload.buffer.is_complete() {
        return Task::ready(Err(anyhow::anyhow!("The upload is incomplete.")));
    }
    let target = upload.target.clone();
    let directory = upload.directory.clone();
    let display_directory = upload.display_directory.clone();
    let name = upload.name.clone();
    let attachment = upload.attachment;
    let bytes = match upload.buffer.take_bytes() {
        Ok(bytes) => bytes,
        Err(error) => return Task::ready(Err(error)),
    };
    let phone = phone.to_string();
    cx.spawn(async move |cx| {
        let saved = match target {
            UploadTarget::Local(fs) => {
                cx.background_spawn(async move {
                    save_local(fs.as_ref(), &directory, &name, &bytes, attachment).await
                })
                .await
            }
            UploadTarget::Remote {
                project,
                worktree,
                directory,
            } => save_remote(project, worktree, &directory, &name, &bytes, attachment, cx).await,
        };
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

fn file_resource_link(abs_path: PathBuf) -> acp::ContentBlock {
    let mention = acp_thread::MentionUri::File { abs_path };
    acp::ContentBlock::ResourceLink(acp::ResourceLink::new(mention.name(), mention.to_uri()))
}

fn remote_attachment_uri(
    roots: &[(String, PathBuf)],
    path: &str,
    resolve_worktree: impl FnOnce(&str, &RelPath) -> Result<PathBuf>,
) -> Result<acp::ContentBlock> {
    let (root_name, relative) = split_project_path(roots, path)?;
    ensure!(!relative.is_empty(), "{path} is not a file");
    let rel_path = RelPath::new(Path::new(relative), PathStyle::Unix)?;
    let abs_path = resolve_worktree(root_name, &rel_path)?;
    Ok(file_resource_link(abs_path))
}

/// The files a chat message attaches, as links the agent can read.
pub(super) fn attachments(
    project: &Entity<Project>,
    roots: &[(String, PathBuf)],
    args: &Value,
    cx: &App,
) -> Result<Vec<acp::ContentBlock>> {
    let Some(value) = args.get("files") else {
        return Ok(Vec::new());
    };
    let paths = value.as_array().context("expected attached file paths")?;
    ensure!(
        !paths.is_empty() && paths.len() <= MAX_ATTACHED_FILES,
        "Attach between one and eight files."
    );
    let is_remote = project.read(cx).remote_connection_options(cx).is_some();
    paths
        .iter()
        .map(|path| {
            let path = path.as_str().context("expected an attached file path")?;
            if is_remote {
                remote_attachment_uri(roots, path, |root_name, rel_path| {
                    let worktree = project
                        .read(cx)
                        .visible_worktrees(cx)
                        .find(|worktree| worktree.read(cx).root_name_str() == root_name)
                        .context("That file is not inside a project folder.")?;
                    Ok(worktree.read(cx).absolutize(rel_path))
                })
            } else {
                let (display, resolved) = resolve_project_path(roots, path)?;
                ensure!(
                    ensure_inside(roots, &resolved)?.is_file(),
                    "{display} is not a file"
                );
                // The path as the project names it, not its canonical form, which
                // on Windows carries a `\\?\` prefix the agent would not recognize.
                Ok(file_resource_link(resolved))
            }
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

    #[gpui::test]
    async fn saving_never_overwrites_and_ignores_attachments(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(workspace::AppState::test);
        let filesystem = app_state.fs.as_fake();
        let directory = PathBuf::from(util::path!("/project"));
        filesystem.insert_tree(&directory, json!({})).await;
        let fs: &dyn Fs = filesystem.as_ref();
        let uploads = directory.join(".praxis").join("uploads");
        assert_eq!(
            save_local(fs, &uploads, "a.txt", b"one", true)
                .await
                .unwrap(),
            "a.txt"
        );
        assert_eq!(
            save_local(fs, &uploads, "a.txt", b"two", true)
                .await
                .unwrap(),
            "a (1).txt"
        );
        assert_eq!(
            filesystem.read_file_sync(uploads.join("a.txt")).unwrap(),
            b"one"
        );
        assert_eq!(
            filesystem
                .read_file_sync(uploads.join("a (1).txt"))
                .unwrap(),
            b"two"
        );
        assert_eq!(
            filesystem
                .read_file_sync(uploads.join(".gitignore"))
                .unwrap(),
            b"*\n"
        );
        assert_eq!(
            save_local(fs, &directory, "README", b"x", false)
                .await
                .unwrap(),
            "README"
        );
        assert!(
            filesystem
                .read_file_sync(directory.join(".gitignore"))
                .is_err(),
            "a plain upload does not create the attachments .gitignore"
        );
    }

    #[gpui::test]
    async fn saving_uses_the_project_filesystem(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(workspace::AppState::test);
        let filesystem = app_state.fs.as_fake();
        let root = PathBuf::from(util::path!("/remote/project"));
        filesystem.insert_tree(&root, json!({})).await;
        let fs: &dyn Fs = filesystem.as_ref();
        let saved = save_local(fs, &root, "a.txt", b"one", false)
            .await
            .expect("saved");
        assert_eq!(saved, "a.txt");
        assert_eq!(
            filesystem.read_file_sync(root.join("a.txt")).expect("read"),
            b"one"
        );
    }

    #[gpui::test]
    async fn chunks_are_ordered_idempotent_and_owner_scoped(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(workspace::AppState::test);
        let filesystem = app_state.fs.as_fake();
        let directory = PathBuf::from(util::path!("/project"));
        filesystem.insert_tree(&directory, json!({})).await;
        let mut uploads = Uploads::default();
        let destination = || Destination {
            target: UploadTarget::Local(filesystem.clone()),
            directory: directory.clone(),
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
        assert_eq!(begin["chunk_bytes"], DEFAULT_CHUNK_BYTES);
        assert_eq!(begin["max_parallel"], MAX_PARALLEL_CHUNKS);
        let chunk = json!({"upload_id": id, "offset": 0, "data": crypto::encode(&[1, 2])});
        assert!(uploads.chunk("other", &chunk).is_err());
        let first = uploads.chunk("phone", &chunk).unwrap();
        assert_eq!(first["next_offset"], 2);
        assert_eq!(first["received_bytes"], 2);
        let duplicate = uploads.chunk("phone", &chunk).unwrap();
        assert_eq!(duplicate["next_offset"], 2);
        assert_eq!(duplicate["received_bytes"], 2);
        let out_of_order = json!({"upload_id": id, "offset": 3, "data": crypto::encode(&[4])});
        let after_gap = uploads.chunk("phone", &out_of_order).unwrap();
        assert_eq!(after_gap["next_offset"], 2);
        assert_eq!(after_gap["received_bytes"], 3);
        let past_end = json!({"upload_id": id, "offset": 4, "data": crypto::encode(&[5])});
        assert!(uploads.chunk("phone", &past_end).is_err());
        let resumed = uploads.begin("phone", destination(), &args).unwrap();
        assert_eq!(resumed["upload_id"], id.as_str());
        assert_eq!(resumed["next_offset"], 2);
        assert_eq!(resumed["received_bytes"], 3);
        assert_eq!(uploads.0.len(), 1);
        let too_large = json!({"name": "big.bin", "size": MAX_UPLOAD_BYTES + 1});
        assert!(uploads.begin("phone", destination(), &too_large).is_err());
    }

    #[gpui::test]
    async fn attachments_stay_inside_the_project(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(workspace::AppState::test);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        std::fs::create_dir_all(root.join(".praxis/uploads")).unwrap();
        std::fs::write(root.join(".praxis/uploads/a.txt"), "a").unwrap();
        std::fs::write(directory.path().join("outside.txt"), "x").unwrap();
        let project = cx.update(|cx| {
            project::Project::local(
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                project::LocalProjectFlags::default(),
                cx,
            )
        });
        let roots = vec![("project".to_string(), root)];
        cx.update(|cx| {
            let links = attachments(
                &project,
                &roots,
                &json!({"files": ["project/.praxis/uploads/a.txt"]}),
                cx,
            )
            .unwrap();
            let [acp::ContentBlock::ResourceLink(link)] = links.as_slice() else {
                panic!("expected one link, got {links:?}");
            };
            assert_eq!(link.name, "a.txt");
            assert!(
                attachments(
                    &project,
                    &roots,
                    &json!({"files": ["project/../outside.txt"]}),
                    cx
                )
                .is_err()
            );
            assert!(
                attachments(&project, &roots, &json!({"files": ["project/.praxis"]}), cx).is_err()
            );
            assert!(attachments(&project, &roots, &json!({"files": []}), cx).is_err());
            assert!(
                attachments(&project, &roots, &json!({}), cx)
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn chunk_buffer_assembles_out_of_order_and_overlapping_chunks() {
        let mut buffer = ChunkBuffer::new(10);
        buffer.insert(6, vec![6, 7, 8, 9]).unwrap();
        assert_eq!(buffer.contiguous_offset(), 0);
        assert_eq!(buffer.received_bytes(), 4);
        assert!(!buffer.is_complete());

        buffer.insert(2, vec![2, 3, 4]).unwrap();
        assert_eq!(buffer.contiguous_offset(), 0);
        assert_eq!(buffer.received_bytes(), 7);

        // Idempotent duplicate and matching partial overlap.
        buffer.insert(2, vec![2, 3, 4]).unwrap();
        buffer.insert(4, vec![4, 5, 6, 7]).unwrap();
        assert_eq!(buffer.contiguous_offset(), 0);
        assert_eq!(buffer.received_bytes(), 8);

        // Conflicting overlap is rejected without corrupting state.
        assert!(buffer.insert(3, vec![99, 4]).is_err());
        assert_eq!(buffer.received_bytes(), 8);

        buffer.insert(0, vec![0, 1, 2]).unwrap();
        assert!(buffer.is_complete());
        assert_eq!(buffer.contiguous_offset(), 10);
        assert_eq!(buffer.received_bytes(), 10);
        assert_eq!(
            buffer.take_bytes().unwrap(),
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
    }

    #[gpui::test]
    async fn file_begin_negotiates_chunk_size_and_parallelism(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(workspace::AppState::test);
        let filesystem = app_state.fs.as_fake();
        let directory = PathBuf::from(util::path!("/project"));
        filesystem.insert_tree(&directory, json!({})).await;
        let mut uploads = Uploads::default();
        let destination = || Destination {
            target: UploadTarget::Local(filesystem.clone()),
            directory: directory.clone(),
            display_directory: "project".into(),
            attachment: false,
        };

        let omitted = uploads
            .begin(
                "phone",
                destination(),
                &json!({"name": "a.bin", "size": 10}),
            )
            .unwrap();
        assert_eq!(omitted["chunk_bytes"], 24 * 1024);
        assert_eq!(omitted["max_parallel"], 6);

        let exact = uploads
            .begin(
                "phone",
                destination(),
                &json!({"name": "b.bin", "size": 10, "preferred_chunk_bytes": 45056}),
            )
            .unwrap();
        assert_eq!(exact["chunk_bytes"], 44 * 1024);
        assert_eq!(exact["max_parallel"], 6);

        let clamped = uploads
            .begin(
                "phone",
                destination(),
                &json!({"name": "c.bin", "size": 10, "preferred_chunk_bytes": 999999}),
            )
            .unwrap();
        assert_eq!(clamped["chunk_bytes"], 44 * 1024);
        assert_eq!(clamped["max_parallel"], 6);
    }

    #[test]
    fn remote_attachment_resolves_posix_uri_without_local_disk() {
        let roots = vec![(
            "my-wsl-repo".to_string(),
            PathBuf::from("/home/user/my-wsl-repo"),
        )];
        let block = remote_attachment_uri(
            &roots,
            "my-wsl-repo/.praxis/uploads/photo.png",
            |root_name, rel_path| {
                assert_eq!(root_name, "my-wsl-repo");
                assert_eq!(rel_path.as_unix_str(), ".praxis/uploads/photo.png");
                Ok(PathBuf::from(format!(
                    "/home/user/my-wsl-repo/{}",
                    rel_path.as_unix_str()
                )))
            },
        )
        .unwrap();
        let acp::ContentBlock::ResourceLink(link) = block else {
            panic!("expected ResourceLink, got {block:?}");
        };
        assert_eq!(link.name, "photo.png");
        assert!(link.uri.starts_with("file://"));
        assert!(
            link.uri
                .contains("/home/user/my-wsl-repo/.praxis/uploads/photo.png")
        );

        assert!(remote_attachment_uri(&roots, "my-wsl-repo", |_, _| unreachable!()).is_err());
        assert!(
            remote_attachment_uri(&roots, "my-wsl-repo/../outside.txt", |_, _| unreachable!())
                .is_err()
        );
    }
}
