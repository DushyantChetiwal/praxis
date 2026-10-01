use super::*;

pub const MAX_FILE_INVENTORY_ENTRIES: usize = 100_000;

/// A fresh, bounded inventory from the filesystem that owns the worktree.
/// Canonical paths are opaque host identities, not paths to open on the client.
#[derive(Debug)]
pub struct FileInventory {
    pub root_path: PathBuf,
    pub files: Vec<String>,
    pub skipped_paths: Vec<String>,
    pub canonical_paths: BTreeMap<String, String>,
    pub entry_count: usize,
}

impl Worktree {
    pub fn check_file_inventory_support(&self) -> Result<()> {
        match self {
            Self::Local(local) => anyhow::ensure!(
                local.scanning_enabled,
                "Project scanning is disabled. Enable project scanning before running the plan."
            ),
            Self::Remote(remote) => {
                anyhow::ensure!(
                    !remote.disconnected,
                    "The project is disconnected. Reconnect to the host before running or resuming the plan."
                );
                match &remote.file_inventory_support {
                    Some(Ok(())) => {}
                    Some(Err(error)) => anyhow::bail!("{error}"),
                    None => anyhow::bail!(
                        "Project file tracking support is still loading. Wait for the project to connect, then retry."
                    ),
                }
            }
        }
        Ok(())
    }

    pub fn negotiate_file_inventory(&mut self, cx: &Context<Self>) -> Task<()> {
        let Self::Remote(remote) = self else {
            return Task::ready(());
        };
        if matches!(remote.file_inventory_support, Some(Ok(()))) && !remote.disconnected {
            return Task::ready(());
        }
        remote.file_inventory_support = None;
        let response = remote.client.request(proto::ExpandProjectEntry {
            project_id: remote.project_id,
            entry_id: 0,
            file_inventory: Some(proto::WorktreeFileInventoryRequest {
                worktree_id: remote.snapshot.id().to_proto(),
                check_support_only: true,
                declared_paths: Vec::new(),
                include_private: false,
            }),
        });
        cx.spawn(async move |this, cx| {
            let support = match response.await {
                Ok(response) if response.supports_file_inventory => Ok(()),
                Ok(_) => Err(
                    "The project host does not support creation tracking. Update Praxis on the host and reconnect before running the plan."
                        .to_string(),
                ),
                Err(error) => Err(format!(
                    "Cannot check project creation tracking: {error}. Reconnect to an updated Praxis host, then retry."
                )),
            };
            this.update(cx, |this, cx| {
                if let Self::Remote(remote) = this {
                    remote.file_inventory_support = Some(support);
                    cx.notify();
                }
            })
            .log_err();
        })
    }

    pub fn file_inventory(
        &self,
        declared_paths: Vec<String>,
        cx: &Context<Self>,
    ) -> Task<Result<FileInventory>> {
        self.file_inventory_internal(declared_paths, true, cx)
    }

    pub(super) fn file_inventory_internal(
        &self,
        declared_paths: Vec<String>,
        include_private: bool,
        cx: &Context<Self>,
    ) -> Task<Result<FileInventory>> {
        if let Err(error) = self.check_file_inventory_support() {
            return Task::ready(Err(error));
        }
        match self {
            Self::Local(_) => {
                let worktree = cx.entity();
                cx.spawn(async move |_, cx| {
                    local_file_inventory(&worktree, declared_paths, include_private, cx).await
                })
            }
            Self::Remote(remote) => {
                let response = remote.client.request(proto::ExpandProjectEntry {
                    project_id: remote.project_id,
                    entry_id: 0,
                    file_inventory: Some(proto::WorktreeFileInventoryRequest {
                        worktree_id: remote.snapshot.id().to_proto(),
                        check_support_only: false,
                        declared_paths,
                        include_private,
                    }),
                });
                cx.spawn(async move |this, cx| {
                    let response = response.await.context(
                        "Cannot inventory remote project files. Reconnect to the host and retry",
                    )?;
                    let inventory = response.file_inventory.context(
                        "The host did not return a file inventory. Update Praxis on the host and reconnect",
                    )?;
                    // As with expansion, do not admit a turn against worktree state
                    // older than the host scan that produced this inventory.
                    this.update(cx, |this, _| {
                        this.check_file_inventory_support()?;
                        Ok::<_, anyhow::Error>(
                            this.as_remote_mut()
                                .context("The project backend changed during inventory")?
                                .wait_for_snapshot(response.worktree_scan_id as usize),
                        )
                    })??
                    .await
                    .context("The project disconnected while waiting for its file inventory. Reconnect and retry")?;
                    this.read_with(cx, |this, _| this.check_file_inventory_support())??;
                    anyhow::ensure!(
                        inventory.entry_count <= MAX_FILE_INVENTORY_ENTRIES as u64
                            && inventory.files.len() <= MAX_FILE_INVENTORY_ENTRIES
                            && inventory.skipped_paths.len() <= MAX_FILE_INVENTORY_ENTRIES
                            && inventory.canonical_paths.len() <= MAX_FILE_INVENTORY_ENTRIES,
                        "Remote file inventory exceeds the project entry budget"
                    );
                    Ok(FileInventory {
                        root_path: PathBuf::from(inventory.root_path),
                        files: inventory.files,
                        skipped_paths: inventory.skipped_paths,
                        canonical_paths: inventory.canonical_paths.into_iter().collect(),
                        entry_count: inventory.entry_count as usize,
                    })
                })
            }
        }
    }
}

fn inventory_relative_path(path: &Path) -> Result<String> {
    let components = path
        .iter()
        .map(|component| {
            let component = component
                .to_str()
                .context("A project file name is not valid UTF-8")?;
            anyhow::ensure!(
                !component.contains('\\'),
                "A project file name contains a literal backslash: {}",
                path.display()
            );
            Ok(component)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(components.join("/"))
}

async fn refresh_inventory_entry(
    worktree: &Entity<Worktree>,
    relative: &str,
    cx: &mut AsyncApp,
) -> Result<Option<Entry>> {
    let path = RelPath::from_unix_str(relative)?.into_arc();
    let task = worktree.update(cx, |worktree, cx| {
        worktree
            .as_local()
            .context("The project backend changed during inventory")
            .map(|local| local.refresh_entry(path, None, cx))
    })?;
    task.await
}

async fn refresh_inventory_ignore_file(
    worktree: &Entity<Worktree>,
    root: &Path,
    directory: &Path,
    fs: &dyn Fs,
    cx: &mut AsyncApp,
) -> Result<()> {
    let ignore_path = directory.join(".gitignore");
    let relative = inventory_relative_path(ignore_path.strip_prefix(root)?)?;
    let path = RelPath::from_unix_str(&relative)?;
    let mut was_present = worktree.read_with(cx, |tree, _| tree.entry_for_path(path).is_some());
    if fs.metadata(&ignore_path).await?.is_some() {
        was_present = true;
        if let Err(error) = fs.load(&ignore_path).await
            && fs.metadata(&ignore_path).await?.is_some()
        {
            return Err(error).context("Cannot read project ignore rules");
        }
    }
    let refreshed = refresh_inventory_entry(worktree, &relative, cx).await;
    let is_present = fs.metadata(&ignore_path).await?.is_some();
    match refreshed {
        Ok(Some(_)) => was_present = true,
        Err(error) if is_present => {
            return Err(error).context("Cannot refresh project ignore rules");
        }
        Ok(None) | Err(_) => {}
    }
    if was_present && !is_present {
        // Targeted refresh removes the entry but only a rescan clears the
        // deleted ignore file's cached matcher when watcher events are delayed.
        worktree.update(cx, |tree, cx| {
            tree.as_local_mut()
                .context("The project backend changed during inventory")?
                .update_abs_path_and_refresh(SanitizedPath::new_arc(root), cx);
            Ok::<_, anyhow::Error>(())
        })?;
        refresh_inventory_entry(worktree, "", cx).await?;
    }
    Ok(())
}

async fn local_file_inventory(
    worktree: &Entity<Worktree>,
    declared_paths: Vec<String>,
    include_private: bool,
    cx: &mut AsyncApp,
) -> Result<FileInventory> {
    anyhow::ensure!(
        declared_paths.len() <= MAX_FILE_INVENTORY_ENTRIES,
        "Too many declared paths for project file inventory"
    );
    // Validate all paths before any join: these may come from an untrusted peer.
    for path in &declared_paths {
        let relative = RelPath::from_unix_str(path)?;
        anyhow::ensure!(
            !relative.is_empty()
                && relative.as_unix_str() == path
                && !path.contains('\\')
                && !path.contains(':'),
            "Invalid declared project path: {path}"
        );
    }
    let (fs, root, settings, initial_scan) = worktree.read_with(cx, |tree, _| {
        let local = tree
            .as_local()
            .context("The project backend changed during inventory")?;
        Ok::<_, anyhow::Error>((
            local.fs().clone(),
            tree.abs_path().to_path_buf(),
            local.settings(),
            local.scan_complete(),
        ))
    })?;
    initial_scan.await;
    let metadata = fs
        .metadata(&root)
        .await
        .with_context(|| format!("Cannot inspect project root {}", root.display()))?
        .with_context(|| format!("Project root {} disappeared", root.display()))?;
    anyhow::ensure!(
        metadata.is_dir,
        "Creation tracking requires a project directory"
    );
    // Refresh barriers alone can succeed after scanner errors; explicit Fs
    // reads on the owning host supply existence and errors, including writes
    // whose watcher events have not yet been delivered.
    let mut inventory = FileInventory {
        root_path: root.clone(),
        files: Vec::new(),
        skipped_paths: Vec::new(),
        canonical_paths: BTreeMap::new(),
        entry_count: 0,
    };
    let mut directories = vec![root.clone()];
    while let Some(directory) = directories.pop() {
        let mut entries = match fs.read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) => {
                if directory != root && fs.metadata(&directory).await?.is_none() {
                    continue;
                }
                return Err(error)
                    .with_context(|| format!("Cannot list {}", directory.display()));
            }
        };
        refresh_inventory_ignore_file(worktree, &root, &directory, fs.as_ref(), cx).await?;
        let mut candidates = Vec::new();
        while let Some(entry) = entries.next().await {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Cannot list {}", directory.display()));
                }
            };
            let relative = inventory_relative_path(entry.strip_prefix(&root)?)?;
            let path = RelPath::from_unix_str(&relative)?;
            // Bound excluded entries and in-flight refresh tasks as well as files.
            inventory.entry_count += 1;
            anyhow::ensure!(
                inventory.entry_count <= MAX_FILE_INVENTORY_ENTRIES,
                "Creation inventory exceeds {MAX_FILE_INVENTORY_ENTRIES} project entries. Exclude generated folders before resuming."
            );
            let private = !include_private
                && worktree.read_with(cx, |tree, _| {
                    tree.as_local()
                        .is_some_and(|local| local.is_path_private(path))
                });
            if settings.is_path_excluded(path)
                || path.file_name().is_some_and(|name| name == ".git")
                || private
            {
                inventory.skipped_paths.push(relative);
                continue;
            }
            let Some(metadata) = fs
                .metadata(&entry)
                .await
                .with_context(|| format!("Cannot inspect {}", entry.display()))?
            else {
                continue;
            };
            if metadata.is_dir && metadata.is_symlink {
                inventory.skipped_paths.push(relative);
                continue;
            }
            let task = worktree.update(cx, |tree, cx| {
                tree.as_local()
                    .context("The project backend changed during inventory")
                    .map(|local| local.refresh_entry(path.into_arc(), None, cx))
            })?;
            candidates.push((entry, relative, metadata, task));
        }
        for (entry, relative, metadata, task) in candidates {
            let classified = match task.await {
                Ok(Some(classified)) => classified,
                Ok(None) => continue,
                Err(error) => {
                    if fs.metadata(&entry).await?.is_none() {
                        continue;
                    }
                    if metadata.is_symlink && fs.canonicalize(&entry).await.is_err() {
                        inventory.skipped_paths.push(relative);
                        continue;
                    }
                    return Err(error).with_context(|| {
                        format!(
                            "Cannot classify {} with project ignore rules",
                            entry.display()
                        )
                    });
                }
            };
            if classified.is_ignored && !classified.is_always_included {
                inventory.skipped_paths.push(relative);
                continue;
            }
            if classified.is_dir() {
                if classified.canonical_path.is_none() {
                    directories.push(entry);
                } else {
                    inventory.skipped_paths.push(relative);
                }
            } else {
                inventory.files.push(relative);
            }
        }
    }
    // Declarations participate only in alias checks, never discovery scope.
    for relative in declared_paths {
        let path = RelPath::from_unix_str(&relative)?;
        if !include_private
            && worktree.read_with(cx, |tree, _| {
                tree.as_local()
                    .is_some_and(|local| local.is_path_private(path))
            })
        {
            continue;
        }
        let absolute = root.join(path.as_std_path());
        let Some(metadata) = fs
            .metadata(&absolute)
            .await
            .with_context(|| format!("Cannot inspect declared file {relative}"))?
        else {
            continue;
        };
        if !metadata.is_dir {
            let canonical = fs
                .canonicalize(&absolute)
                .await
                .with_context(|| format!("Cannot resolve declared file {relative}"))?;
            inventory.canonical_paths.insert(
                relative,
                canonical
                    .to_str()
                    .context("A canonical project path is not valid UTF-8")?
                    .to_owned(),
            );
        }
    }
    worktree.read_with(cx, |tree, _| {
        tree.check_file_inventory_support()?;
        anyhow::ensure!(
            tree.abs_path().as_path() == root,
            "Project root changed while scanning. Restore it and retry."
        );
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(inventory)
}
