//! Files a paired phone uploads into a project, either into the folder shown
//! in its Files tab or as attachments to a chat message.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, ensure};
use fs::Fs;
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, Task};
use project::{Project, Worktree};
use serde_json::{Value, json};
use util::rel_path::RelPath;

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
    target: UploadTarget,
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
/// Where an upload is written: this computer's filesystem, or a worktree on
/// the computer the project lives on.
#[derive(Clone)]
pub(super) enum UploadTarget {
    Local(Arc<dyn Fs>),
    Remote(Entity<Worktree>),
}

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
    let (directory, display_directory, attachment) =
        if args.get("attachment").and_then(Value::as_bool) == Some(true) {
            let (root_name, root) = roots
                .first()
                .context("Open a project folder before attaching files.")?;
            (
                root.join(ATTACHMENTS_DIR),
                format!("{root_name}/{ATTACHMENTS_DIR}"),
                true,
            )
        } else {
            let (display, path) = resolve_project_path(roots, required(args, "path")?)?;
            (ensure_inside(roots, &path)?, display, false)
        };
    let target = if project.read(cx).remote_connection_options(cx).is_some() {
        let worktree = project
            .read(cx)
            .worktrees(cx)
            .find(|worktree| directory.starts_with(worktree.read(cx).abs_path()))
            .context("That folder is not inside a project folder.")?;
        UploadTarget::Remote(worktree)
    } else {
        UploadTarget::Local(project.read(cx).fs().clone())
    };
    Ok(Destination {
        target,
        directory,
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

/// Writes the file into a worktree on the computer the project lives on, which
/// is the only way to save an upload there.
async fn save_remote(
    worktree: Entity<Worktree>,
    directory: &Path,
    name: &str,
    bytes: &[u8],
    attachment: bool,
    cx: &mut AsyncApp,
) -> Result<String> {
    let (root, path_style) = worktree.read_with(cx, |worktree, _| {
        (worktree.abs_path().to_path_buf(), worktree.path_style())
    });
    let relative = directory
        .strip_prefix(&root)
        .with_context(|| format!("{} is not inside the project", directory.display()))?;
    let directory_path = RelPath::new(relative, path_style)?;

    if attachment {
        for folder in [".praxis", ".praxis/uploads"] {
            let path = RelPath::new(Path::new(folder), path_style)?;
            if worktree.read_with(cx, |worktree, _| worktree.entry_for_path(&path).is_none()) {
                worktree
                    .update(cx, |worktree, cx| {
                        worktree.create_entry(path.into_arc(), true, None, cx)
                    })
                    .await?;
            }
        }
        // Uploads are the user's files, not the project's.
        let ignore = RelPath::new(Path::new(".praxis/uploads/.gitignore"), path_style)?;
        if worktree.read_with(cx, |worktree, _| worktree.entry_for_path(&ignore).is_none()) {
            worktree
                .update(cx, |worktree, cx| {
                    worktree.create_entry(ignore.into_arc(), false, Some(b"*\n".to_vec()), cx)
                })
                .await?;
        }
    }

    let (stem, extension) = split_extension(name);
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){extension}")
        };
        let path = RelPath::new(&directory_path.as_std_path().join(&candidate), path_style)?;
        if worktree.read_with(cx, |worktree, _| worktree.entry_for_path(&path).is_some()) {
            continue;
        }
        match worktree
            .update(cx, |worktree, cx| {
                worktree.create_entry(path.into_arc(), false, Some(bytes.to_vec()), cx)
            })
            .await
        {
            Ok(_) => return Ok(candidate),
            // The snapshot can lag behind a file another upload just wrote.
            Err(error) if format!("{error:#}").contains("already exists") => continue,
            Err(error) => return Err(error),
        }
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
                target: destination.target.clone(),
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
    let target = upload.target.clone();
    let directory = upload.directory.clone();
    let display_directory = upload.display_directory.clone();
    let name = upload.name.clone();
    let attachment = upload.attachment;
    let bytes = std::mem::take(&mut upload.bytes);
    let phone = phone.to_string();
    cx.spawn(async move |cx| {
        let saved = match target {
            UploadTarget::Local(fs) => {
                cx.background_spawn(async move {
                    save_local(fs.as_ref(), &directory, &name, &bytes, attachment).await
                })
                .await
            }
            UploadTarget::Remote(worktree) => {
                save_remote(worktree, &directory, &name, &bytes, attachment, cx).await
            }
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

    #[gpui::test]
    async fn saving_never_overwrites_and_ignores_attachments(cx: &mut gpui::TestAppContext) {
        let app_state = cx.update(workspace::AppState::test);
        let filesystem = app_state.fs.as_fake();
        let directory = PathBuf::from(util::path!("/project"));
        filesystem.insert_tree(&directory, json!({})).await;
        let fs: &dyn Fs = filesystem.as_ref();
        let uploads = directory.join(".praxis").join("uploads");
        assert_eq!(
            save_local(fs, &uploads, "a.txt", b"one", true).await.unwrap(),
            "a.txt"
        );
        assert_eq!(
            save_local(fs, &uploads, "a.txt", b"two", true).await.unwrap(),
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
            save_local(fs, &directory, "README", b"x", false).await.unwrap(),
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
        let roots = vec![("project".to_string(), root)];
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
