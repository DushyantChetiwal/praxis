mod patch;

use agent_client_protocol::schema::v2 as acp_v2;
use anyhow::{Result, anyhow, ensure};
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use gpui::{App, AppContext, AsyncApp, Context, Entity, SharedString, Subscription, Task};
use itertools::Itertools;
use language::{
    Anchor, Bias, Buffer, Capability, LanguageRegistry, OffsetRangeExt as _, Point, TextBuffer,
};
use markdown::Markdown;
use multi_buffer::{MultiBuffer, PathKey, excerpt_context_lines};
use std::{cmp::Reverse, ops::Range, path::Path, sync::Arc};
use util::ResultExt;

#[derive(Debug)]
pub struct DiffPatch {
    pub files: Vec<DiffPatchFile>,
    pub fallback: Option<Entity<Markdown>>,
}

#[derive(Debug)]
pub struct DiffPatchFile {
    pub change_index: usize,
    pub hunks: Vec<DiffPatchHunk>,
}

#[derive(Debug)]
pub struct DiffPatchHunk {
    pub header: SharedString,
    pub buffer: Entity<MultiBuffer>,
    _update_diff: Task<()>,
}

impl DiffPatch {
    pub(crate) fn new(
        source: &acp_v2::Diff,
        language_registry: &Arc<LanguageRegistry>,
        cx: &mut App,
    ) -> Self {
        let mut files = source
            .changes
            .iter()
            .enumerate()
            .map(|(change_index, _)| DiffPatchFile {
                change_index,
                hunks: Vec::new(),
            })
            .collect::<Vec<_>>();
        if source.patch.is_none() {
            return Self {
                files,
                fallback: None,
            };
        }

        match Self::parse(source) {
            Ok(parsed) => {
                for (change_index, file) in parsed {
                    let path = file.new_path.or(file.old_path).unwrap_or_default();
                    if let Some(render) = files.get_mut(change_index) {
                        render.hunks = file
                            .hunks
                            .into_iter()
                            .map(|hunk| {
                                DiffPatchHunk::new(hunk, &path, language_registry.clone(), cx)
                            })
                            .collect();
                    }
                }
                Self {
                    files,
                    fallback: None,
                }
            }
            Err(error) => {
                let preview = format!(
                    "Diff preview unavailable: {error}\n\n{}",
                    crate::ToolCallContent::patch_preview(source)
                );
                Self {
                    files,
                    fallback: Some(crate::ContentBlock::create_markdown(
                        preview,
                        language_registry,
                        cx,
                    )),
                }
            }
        }
    }

    fn parse(source: &acp_v2::Diff) -> Result<Vec<(usize, patch::PatchFile)>> {
        let patch = source
            .patch
            .as_ref()
            .ok_or_else(|| anyhow!("no patch supplied"))?;
        ensure!(
            patch.format == acp_v2::DiffPatchFormat::GitPatch,
            "unsupported patch format"
        );
        ensure!(
            !source.changes.is_empty(),
            "patch has no reported file changes"
        );
        let mut change_indices: HashMap<_, Option<usize>> = HashMap::default();
        for (index, change) in source.changes.iter().enumerate() {
            if let Some(paths) = diff_change_paths(change) {
                change_indices
                    .entry(paths)
                    .and_modify(|index| *index = None)
                    .or_insert(Some(index));
            }
        }
        let mut seen = HashSet::default();
        patch::parse_patch(&patch.text)?
            .into_iter()
            .map(|mut file| {
                let paths = (
                    file.old_path.as_deref().map(Path::new),
                    file.new_path.as_deref().map(Path::new),
                );
                let anonymous = paths == (None, None);
                let paths = if anonymous {
                    let [change] = source.changes.as_slice() else {
                        return Err(anyhow!(
                            "patch without filenames requires exactly one file change"
                        ));
                    };
                    diff_change_paths(change)
                        .ok_or_else(|| anyhow!("unsupported file operation"))?
                } else {
                    paths
                };
                let index = change_indices
                    .get(&paths)
                    .copied()
                    .flatten()
                    .ok_or_else(|| {
                        anyhow!(
                            "patch paths are ambiguous or do not match the reported file changes"
                        )
                    })?;
                ensure!(seen.insert(index), "repeated patch file");
                let change = source
                    .changes
                    .get(index)
                    .ok_or_else(|| anyhow!("missing file change"))?;
                ensure!(
                    matches!(change.file_type, None | Some(acp_v2::DiffFileType::Text)),
                    "text patch supplied for a non-text file"
                );
                if anonymous && let Some((old, new)) = diff_change_paths(change) {
                    file.old_path = old.map(|path| path.to_string_lossy().into_owned());
                    file.new_path = new.map(|path| path.to_string_lossy().into_owned());
                }
                ensure!(
                    file.hunks.iter().all(|hunk| {
                        (file.old_path.is_some() || hunk.old_count == 0)
                            && (file.new_path.is_some() || hunk.new_count == 0)
                    }),
                    "hunk contents conflict with the reported file operation"
                );
                Ok((index, file))
            })
            .collect()
    }
}

impl DiffPatchHunk {
    fn new(
        hunk: patch::PatchHunk,
        path: &str,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut App,
    ) -> Self {
        let range_label = |start, count| {
            if count == 1 {
                format!("{start}")
            } else {
                format!("{start},{count}")
            }
        };
        let header = format!(
            "@@ -{} +{} @@",
            range_label(hunk.old_start, hunk.old_count),
            range_label(hunk.new_start, hunk.new_count),
        )
        .into();
        let new_buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(hunk.new_text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::without_headers(Capability::ReadOnly);
            multibuffer.set_excerpts_for_path(
                PathKey::for_buffer(&new_buffer, cx),
                new_buffer.clone(),
                [Point::new(0, 0)..new_buffer.read(cx).max_point()],
                0,
                cx,
            );
            multibuffer
        });
        let task = cx.spawn({
            let multibuffer = multibuffer.clone();
            let path = path.to_owned();
            async move |cx| {
                let language = language_registry
                    .load_language_for_file_path(Path::new(&path))
                    .await
                    .log_err();
                new_buffer.update(cx, |buffer, cx| buffer.set_language(language.clone(), cx));
                let snapshot = new_buffer.read_with(cx, |buffer, _| buffer.snapshot());
                let diff = cx.new(|cx| {
                    // A hunk is only a snippet: it must never offer restore/apply operations.
                    BufferDiff::new(&snapshot, language, Some(language_registry), cx)
                });
                diff.update(cx, |diff, cx| {
                    diff.set_base_text(Some(hunk.old_text.into()), snapshot.text, cx)
                })
                .await;
                multibuffer.update(cx, |buffer, cx| {
                    buffer.add_diff(diff, cx);
                    buffer.set_all_diff_hunks_expanded(cx);
                });
            }
        });
        Self {
            header,
            buffer: multibuffer,
            _update_diff: task,
        }
    }
}

fn diff_change_paths(change: &acp_v2::DiffChange) -> Option<(Option<&Path>, Option<&Path>)> {
    match &change.operation {
        acp_v2::DiffChangeOperation::Add(change) => Some((None, Some(&change.path.0))),
        acp_v2::DiffChangeOperation::Delete(change) => Some((Some(&change.path.0), None)),
        acp_v2::DiffChangeOperation::Modify(change) => {
            Some((Some(&change.path.0), Some(&change.path.0)))
        }
        acp_v2::DiffChangeOperation::Move(change) | acp_v2::DiffChangeOperation::Copy(change) => {
            Some((Some(&change.old_path.0), Some(&change.path.0)))
        }
        _ => None,
    }
}

pub fn diff_change_label(change: &acp_v2::DiffChange) -> String {
    match &change.operation {
        acp_v2::DiffChangeOperation::Add(change) => format!("Added {}", change.path.0.display()),
        acp_v2::DiffChangeOperation::Delete(change) => {
            format!("Deleted {}", change.path.0.display())
        }
        acp_v2::DiffChangeOperation::Modify(change) => {
            format!("Modified {}", change.path.0.display())
        }
        acp_v2::DiffChangeOperation::Move(change) => format!(
            "Moved {} → {}",
            change.old_path.0.display(),
            change.path.0.display()
        ),
        acp_v2::DiffChangeOperation::Copy(change) => format!(
            "Copied {} → {}",
            change.old_path.0.display(),
            change.path.0.display()
        ),
        _ => "Unsupported file operation".to_owned(),
    }
}

pub enum Diff {
    Pending(PendingDiff),
    Finalized(FinalizedDiff),
    Historical(HistoricalDiff),
}

impl Diff {
    pub fn finalized(
        path: String,
        old_text: Option<String>,
        new_text: String,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::Finalized(Self::finalized_data(
            path,
            old_text,
            new_text,
            language_registry,
            cx,
        ))
    }

    fn finalized_data(
        path: String,
        old_text: Option<String>,
        new_text: String,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> FinalizedDiff {
        let multibuffer = cx.new(|_cx| MultiBuffer::without_headers(Capability::ReadOnly));
        let new_buffer = cx.new(|cx| Buffer::local(new_text, cx));
        let base_text_exists = old_text.is_some();
        let base_text = old_text.clone().unwrap_or(String::new()).into();
        let task = cx.spawn({
            let multibuffer = multibuffer.clone();
            let path = path.clone();
            let buffer = new_buffer.clone();
            async move |this, cx| {
                let language = language_registry
                    .load_language_for_file_path(Path::new(&path))
                    .await
                    .log_err();

                buffer.update(cx, |buffer, cx| buffer.set_language(language.clone(), cx));
                buffer.update(cx, |buffer, _| buffer.parsing_idle()).await;

                let diff = build_buffer_diff(
                    old_text.unwrap_or("".into()).into(),
                    base_text_exists,
                    &buffer,
                    cx,
                )
                .await?;

                multibuffer.update(cx, |multibuffer, cx| {
                    let hunk_ranges = {
                        let buffer = buffer.read(cx);
                        diff.read(cx)
                            .snapshot(cx)
                            .hunks_intersecting_range(
                                Anchor::min_for_buffer(buffer.remote_id())
                                    ..Anchor::max_for_buffer(buffer.remote_id()),
                                buffer,
                            )
                            .map(|diff_hunk| diff_hunk.buffer_range.to_point(buffer))
                            .collect::<Vec<_>>()
                    };

                    multibuffer.set_excerpts_for_path(
                        PathKey::for_buffer(&buffer, cx),
                        buffer.clone(),
                        hunk_ranges,
                        excerpt_context_lines(cx),
                        cx,
                    );
                    multibuffer.add_diff(diff, cx);
                });

                this.update(cx, |_, cx| cx.notify())?;
                anyhow::Ok(())
            }
        });

        FinalizedDiff {
            multibuffer,
            path,
            base_text,
            unified_diff: None,
            new_buffer,
            _update_diff: task,
        }
    }

    /// Saved edits must not allocate buffers, parsers, or editor state during replay.
    pub fn historical(
        path: String,
        old_text: Option<String>,
        new_text: String,
        language_registry: Arc<LanguageRegistry>,
    ) -> Self {
        Self::Historical(HistoricalDiff {
            path,
            base_text_exists: old_text.is_some(),
            base_text: old_text.unwrap_or_default().into(),
            new_text: new_text.into(),
            unified_diff: None,
            language_registry,
            view: None,
        })
    }

    /// A saved edit that kept only its unified diff. Opening it shows the
    /// same hunks without either full version of the file.
    pub fn historical_from_unified_diff(
        path: String,
        unified_diff: &str,
        language_registry: Arc<LanguageRegistry>,
    ) -> Result<Self> {
        // The rebuilt texts are padded to each hunk's rows, often thousands of
        // empty lines, so they are rebuilt again only when the edit is opened.
        sparse_texts_from_unified_diff(unified_diff)?;
        Ok(Self::Historical(HistoricalDiff {
            path,
            base_text_exists: true,
            base_text: Arc::from(""),
            new_text: Arc::from(""),
            unified_diff: Some(unified_diff.into()),
            language_registry,
            view: None,
        }))
    }

    pub fn is_historical(&self) -> bool {
        matches!(self, Self::Historical(_))
    }

    pub fn materialize(&mut self, cx: &mut Context<Self>) -> bool {
        let Self::Historical(snapshot) = self else {
            return false;
        };
        if snapshot.view.is_some() {
            return false;
        }
        let (base_text, new_text) = match snapshot.texts() {
            Ok(texts) => texts,
            Err(error) => {
                log::error!("Could not show a saved edit of {}: {error:#}", snapshot.path);
                return false;
            }
        };
        snapshot.view = Some(Self::finalized_data(
            snapshot.path.clone(),
            snapshot.base_text_exists.then_some(base_text),
            new_text,
            snapshot.language_registry.clone(),
            cx,
        ));
        cx.notify();
        true
    }

    pub fn release_render_cache(&mut self, cx: &mut Context<Self>) -> bool {
        let Self::Historical(snapshot) = self else {
            return false;
        };
        if snapshot.view.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    pub fn is_loading(&self) -> bool {
        match self {
            Self::Historical(snapshot) => snapshot
                .view
                .as_ref()
                .is_some_and(|view| !view._update_diff.is_ready()),
            _ => false,
        }
    }

    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<Self>) -> Self {
        let buffer_text_snapshot = buffer.read(cx).text_snapshot();
        let language = buffer.read(cx).language().cloned();
        let language_registry = buffer.read(cx).language_registry();
        let buffer_diff = cx.new(|cx| {
            let mut diff =
                BufferDiff::new_unchanged(&buffer_text_snapshot, language, language_registry, cx);
            diff.set_operations(Arc::new(buffer_diff::RestoreDiffOperations));
            diff
        });

        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::without_headers(Capability::ReadOnly);
            multibuffer.add_diff(buffer_diff.clone(), cx);
            multibuffer
        });

        Self::Pending(PendingDiff {
            multibuffer,
            base_text: Arc::from(buffer_text_snapshot.text().as_str()),
            _subscription: cx.observe(&buffer, |this, _, cx| {
                if let Diff::Pending(diff) = this {
                    diff.update(cx);
                }
            }),
            new_buffer: buffer,
            diff: buffer_diff,
            revealed_ranges: Vec::new(),
            update_diff: Task::ready(Ok(())),
        })
    }

    pub fn reveal_range(&mut self, range: Range<Anchor>, cx: &mut Context<Self>) {
        if let Self::Pending(diff) = self {
            diff.reveal_range(range, cx);
        }
    }

    pub fn finalize(&mut self, cx: &mut Context<Self>) {
        if let Self::Pending(diff) = self {
            *self = Self::Finalized(diff.finalize(cx));
        }
    }

    /// Returns the original text before any edits were applied.
    pub fn base_text(&self) -> &Arc<str> {
        match self {
            Self::Pending(PendingDiff { base_text, .. }) => base_text,
            Self::Finalized(FinalizedDiff { base_text, .. }) => base_text,
            Self::Historical(snapshot) => snapshot
                .view
                .as_ref()
                .map_or(&snapshot.base_text, |view| &view.base_text),
        }
    }

    /// An unopened historical edit has no buffer until explicitly materialized.
    pub fn buffer(&self) -> Option<&Entity<Buffer>> {
        match self {
            Self::Pending(PendingDiff { new_buffer, .. }) => Some(new_buffer),
            Self::Finalized(FinalizedDiff { new_buffer, .. }) => Some(new_buffer),
            Self::Historical(snapshot) => snapshot.view.as_ref().map(|view| &view.new_buffer),
        }
    }

    pub fn file_path(&self, cx: &App) -> Option<String> {
        match self {
            Self::Pending(PendingDiff { new_buffer, .. }) => new_buffer
                .read(cx)
                .file()
                .map(|file| file.full_path(cx).to_string_lossy().into_owned()),
            Self::Finalized(FinalizedDiff { path, .. }) => Some(path.clone()),
            Self::Historical(HistoricalDiff { path, .. }) => Some(path.clone()),
        }
    }

    pub fn multibuffer(&self) -> Option<&Entity<MultiBuffer>> {
        match self {
            Self::Pending(PendingDiff { multibuffer, .. }) => Some(multibuffer),
            Self::Finalized(FinalizedDiff { multibuffer, .. }) => Some(multibuffer),
            Self::Historical(snapshot) => snapshot.view.as_ref().map(|view| &view.multibuffer),
        }
    }

    pub fn to_markdown(&self, cx: &App) -> String {
        if let Self::Finalized(FinalizedDiff {
            path,
            unified_diff: Some(unified_diff),
            ..
        }) = self
        {
            return format!("Diff: {path}\n```diff\n{}\n```\n", unified_diff.trim_end());
        }
        if let Self::Historical(snapshot) = self {
            return match &snapshot.unified_diff {
                Some(unified_diff) => format!(
                    "Diff: {}\n```diff\n{}\n```\n",
                    snapshot.path,
                    unified_diff.trim_end()
                ),
                None => format!("Diff: {}\n```\n{}\n```\n", snapshot.path, snapshot.new_text),
            };
        }
        let buffer_text = self
            .multibuffer()
            .map(|buffer| {
                buffer
                    .read(cx)
                    .all_buffers()
                    .iter()
                    .map(|buffer| buffer.read(cx).text())
                    .join("\n")
            })
            .unwrap_or_default();
        let path = match self {
            Diff::Pending(PendingDiff {
                new_buffer: buffer, ..
            }) => buffer
                .read(cx)
                .file()
                .map(|file| file.path().display(file.path_style(cx))),
            Diff::Finalized(FinalizedDiff { path, .. }) => Some(path.as_str().into()),
            Diff::Historical(HistoricalDiff { path, .. }) => Some(path.as_str().into()),
        };
        format!(
            "Diff: {}\n```\n{}\n```\n",
            path.unwrap_or(MultiBuffer::DEFAULT_TITLE.into()),
            buffer_text
        )
    }

    pub fn has_revealed_range(&self, cx: &App) -> bool {
        self.multibuffer()
            .is_some_and(|buffer| !buffer.read(cx).is_empty())
    }

    pub fn needs_update(&self, old_text: &str, new_text: &str, cx: &App) -> bool {
        match self {
            Diff::Pending(PendingDiff {
                base_text,
                new_buffer,
                ..
            }) => {
                base_text.as_ref() != old_text
                    || !new_buffer.read(cx).as_rope().chunks().equals_str(new_text)
            }
            Diff::Finalized(FinalizedDiff {
                base_text,
                new_buffer,
                ..
            }) => {
                base_text.as_ref() != old_text
                    || !new_buffer.read(cx).as_rope().chunks().equals_str(new_text)
            }
            Diff::Historical(snapshot) => match snapshot.texts() {
                Ok((base_text, text)) => base_text != old_text || text != new_text,
                Err(_) => true,
            },
        }
    }
}

pub struct PendingDiff {
    multibuffer: Entity<MultiBuffer>,
    base_text: Arc<str>,
    new_buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    revealed_ranges: Vec<Range<Anchor>>,
    _subscription: Subscription,
    update_diff: Task<Result<()>>,
}

impl PendingDiff {
    pub fn update(&mut self, cx: &mut Context<Diff>) {
        let buffer = self.new_buffer.clone();
        let buffer_diff = self.diff.clone();
        let base_text = self.base_text.clone();
        self.update_diff = cx.spawn(async move |diff, cx| {
            let text_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
            let base_text_snapshot = buffer_diff.read_with(cx, |diff, cx| diff.base_text(cx));
            let update = buffer_diff
                .update(cx, |diff, cx| {
                    diff.update_diff(
                        text_snapshot.clone(),
                        &base_text_snapshot,
                        Some(base_text.clone()),
                        cx,
                    )
                })
                .await;
            buffer_diff.update(cx, |diff, cx| {
                diff.set_snapshot(update.clone(), cx);
            });
            diff.update(cx, |diff, cx| {
                if let Diff::Pending(diff) = diff {
                    diff.update_visible_ranges(cx);
                }
            })
        });
    }

    pub fn reveal_range(&mut self, range: Range<Anchor>, cx: &mut Context<Diff>) {
        self.revealed_ranges.push(range);
        self.update_visible_ranges(cx);
    }

    fn finalize(&self, cx: &mut Context<Diff>) -> FinalizedDiff {
        let ranges = self.excerpt_ranges(cx);
        let base_text = self.base_text.clone();
        let new_buffer = self.new_buffer.read(cx);

        let path = new_buffer
            .file()
            .map(|file| file.path().display(file.path_style(cx)))
            .unwrap_or(MultiBuffer::DEFAULT_TITLE.into())
            .into();
        let replica_id = new_buffer.replica_id();
        let line_ending = new_buffer.line_ending();
        let language = new_buffer.language().cloned();
        let new_text = new_buffer.as_rope().clone();
        let context_lines = excerpt_context_lines(cx);

        let update_diff = cx.spawn(async move |this, cx| {
            // A finished edit only ever shows its hunks. Copies of both whole
            // versions would cost two syntax trees of the file for every edit
            // in the conversation, so keep just the hunks at their own rows.
            let (shown_base_text, shown_new_text, unified_diff) = cx
                .background_spawn(async move {
                    let new_text = new_text.to_string();
                    let unified_diff = language::unified_diff_with_context(
                        &base_text,
                        &new_text,
                        0,
                        0,
                        context_lines,
                    );
                    match sparse_texts_from_unified_diff(&unified_diff) {
                        Ok((shown_base_text, shown_new_text)) => {
                            (shown_base_text, shown_new_text, Some(unified_diff))
                        }
                        Err(error) => {
                            log::debug!("Showing a whole file for an edit: {error:#}");
                            (base_text.to_string(), new_text, None)
                        }
                    }
                })
                .await;

            let buffer = cx.new(|cx| {
                let text_buffer = TextBuffer::new_normalized(
                    replica_id,
                    cx.entity_id().as_non_zero_u64().into(),
                    line_ending,
                    text::Rope::from(shown_new_text.as_str()),
                );
                let mut buffer = Buffer::build(text_buffer, None, Capability::ReadWrite, cx);
                buffer.set_language(language, cx);
                buffer
            });
            buffer.update(cx, |buffer, _| buffer.parsing_idle()).await;
            let buffer_diff = build_buffer_diff(shown_base_text.into(), true, &buffer, cx).await?;

            this.update(cx, |this, cx| {
                if let Diff::Finalized(finalized) = this {
                    finalized.unified_diff = unified_diff.map(Arc::from);
                }
                let multibuffer = this
                    .multibuffer()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Live diff lost its multibuffer"))?;
                multibuffer.update(cx, |multibuffer, cx| {
                    let ranges = {
                        let snapshot = buffer.read(cx).snapshot();
                        ranges
                            .into_iter()
                            .map(|range| {
                                snapshot.clip_point(range.start, Bias::Left)
                                    ..snapshot.clip_point(range.end, Bias::Left)
                            })
                            .collect::<Vec<_>>()
                    };
                    let path_key = PathKey::for_buffer(&buffer, cx);
                    multibuffer.clear(cx);
                    multibuffer.set_excerpts_for_path(
                        path_key,
                        buffer,
                        ranges,
                        excerpt_context_lines(cx),
                        cx,
                    );
                    multibuffer.add_diff(buffer_diff.clone(), cx);
                });

                cx.notify();
                anyhow::Ok(())
            })?
        });

        FinalizedDiff {
            path,
            base_text: self.base_text.clone(),
            unified_diff: None,
            multibuffer: self.multibuffer.clone(),
            new_buffer: self.new_buffer.clone(),
            _update_diff: update_diff,
        }
    }

    fn update_visible_ranges(&mut self, cx: &mut Context<Diff>) {
        let ranges = self.excerpt_ranges(cx);
        self.multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.set_excerpts_for_path(
                PathKey::for_buffer(&self.new_buffer, cx),
                self.new_buffer.clone(),
                ranges,
                excerpt_context_lines(cx),
                cx,
            );
            let end = multibuffer.len(cx);
            Some(multibuffer.snapshot(cx).offset_to_point(end).row + 1)
        });
        cx.notify();
    }

    fn excerpt_ranges(&self, cx: &App) -> Vec<Range<Point>> {
        let buffer = self.new_buffer.read(cx);
        let mut ranges = self
            .diff
            .read(cx)
            .snapshot(cx)
            .hunks_intersecting_range(
                Anchor::min_for_buffer(buffer.remote_id())
                    ..Anchor::max_for_buffer(buffer.remote_id()),
                buffer,
            )
            .map(|diff_hunk| diff_hunk.buffer_range.to_point(buffer))
            .collect::<Vec<_>>();
        ranges.extend(
            self.revealed_ranges
                .iter()
                .map(|range| range.to_point(buffer)),
        );
        ranges.sort_unstable_by_key(|range| (range.start, Reverse(range.end)));

        // Merge adjacent ranges
        let mut ranges = ranges.into_iter().peekable();
        let mut merged_ranges = Vec::new();
        while let Some(mut range) = ranges.next() {
            while let Some(next_range) = ranges.peek() {
                if range.end >= next_range.start {
                    range.end = range.end.max(next_range.end);
                    ranges.next();
                } else {
                    break;
                }
            }

            merged_ranges.push(range);
        }
        merged_ranges
    }
}

pub struct HistoricalDiff {
    path: String,
    /// Empty when `unified_diff` is set.
    base_text: Arc<str>,
    base_text_exists: bool,
    /// Empty when `unified_diff` is set.
    new_text: Arc<str>,
    /// Set when the texts are rebuilt from this diff and hold only its hunks.
    unified_diff: Option<Arc<str>>,
    language_registry: Arc<LanguageRegistry>,
    view: Option<FinalizedDiff>,
}

impl HistoricalDiff {
    fn texts(&self) -> Result<(String, String)> {
        match &self.unified_diff {
            Some(unified_diff) => sparse_texts_from_unified_diff(unified_diff),
            None => Ok((self.base_text.to_string(), self.new_text.to_string())),
        }
    }
}

/// Rebuilt texts are padded with empty lines up to this row at most, so a
/// corrupt hunk header can't allocate without bound.
const MAX_REBUILT_ROWS: u32 = 1 << 22;

/// Places each hunk at its own line numbers, with empty lines between hunks,
/// so the rebuilt texts differ exactly where the saved edit did and hunks far
/// apart still show as separate excerpts.
fn sparse_texts_from_unified_diff(unified_diff: &str) -> Result<(String, String)> {
    let mut base_text = String::new();
    let mut new_text = String::new();
    if unified_diff.trim().is_empty() {
        return Ok((base_text, new_text));
    }
    let files = patch::parse_patch(unified_diff)?;
    ensure!(files.len() <= 1, "a saved edit's diff covers one file");
    let mut base_rows = 0;
    let mut new_rows = 0;
    for hunk in files.iter().flat_map(|file| &file.hunks) {
        // The unchanged lines before a hunk are as many on either side, so a
        // side with lines places both. Diff writers disagree on where an
        // empty side starts.
        let gap = if hunk.old_count > 0 {
            hunk.old_start.saturating_sub(1).checked_sub(base_rows)
        } else {
            hunk.new_start.saturating_sub(1).checked_sub(new_rows)
        }
        .ok_or_else(|| anyhow!("hunks overlap or are out of order"))?;
        push_hunk_side(
            &mut base_text,
            &mut base_rows,
            gap,
            hunk.old_count,
            &hunk.old_text,
        )?;
        push_hunk_side(
            &mut new_text,
            &mut new_rows,
            gap,
            hunk.new_count,
            &hunk.new_text,
        )?;
    }
    Ok((base_text, new_text))
}

fn push_hunk_side(
    text: &mut String,
    written_rows: &mut u32,
    gap: u32,
    count: u32,
    lines: &str,
) -> Result<()> {
    let end_row = written_rows
        .checked_add(gap)
        .and_then(|first_row| first_row.checked_add(count))
        .filter(|end_row| *end_row <= MAX_REBUILT_ROWS)
        .ok_or_else(|| anyhow!("hunk is beyond the rebuilt text's limit"))?;
    for _ in 0..gap {
        text.push('\n');
    }
    text.push_str(lines);
    if !lines.is_empty() && !lines.ends_with('\n') {
        text.push('\n');
    }
    *written_rows = end_row;
    Ok(())
}

pub struct FinalizedDiff {
    path: String,
    base_text: Arc<str>,
    /// Set once a finished live edit has dropped the rest of the file.
    unified_diff: Option<Arc<str>>,
    new_buffer: Entity<Buffer>,
    multibuffer: Entity<MultiBuffer>,
    _update_diff: Task<Result<()>>,
}

async fn build_buffer_diff(
    old_text: Arc<str>,
    base_text_exists: bool,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let language_registry = cx.update(|cx| buffer.read(cx).language_registry());
    let buffer = cx.update(|cx| buffer.read(cx).snapshot());
    let base_text = base_text_exists.then(|| old_text);

    let diff = cx.new(|cx| {
        let mut diff = BufferDiff::new(&buffer, language, language_registry, cx);
        diff.set_operations(Arc::new(buffer_diff::RestoreDiffOperations));
        diff
    });
    diff.update(cx, |diff, cx| {
        diff.set_base_text(base_text, buffer.text, cx)
    })
    .await;
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use indoc::indoc;
    use language::{Buffer, LanguageRegistry};
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;

    use crate::Diff;

    fn historical_diff_languages(cx: &mut TestAppContext) -> Arc<LanguageRegistry> {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            Arc::new(LanguageRegistry::test(cx.background_executor().clone()))
        })
    }

    #[gpui::test]
    fn historical_diffs_restore_without_buffers_and_preserve_text(cx: &mut TestAppContext) {
        let languages = historical_diff_languages(cx);
        let old_text = "Before Ω\n".repeat(128);
        let new_text = "After 日本語\n".repeat(128);
        let diffs = (0..256)
            .map(|index| {
                cx.new(|_| {
                    Diff::historical(
                        format!("saved-{index}.txt"),
                        Some(old_text.clone()),
                        new_text.clone(),
                        languages.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        cx.run_until_parked();

        for (index, diff) in diffs.iter().enumerate() {
            diff.read_with(cx, |diff, cx| {
                assert!(diff.is_historical());
                assert!(diff.buffer().is_none());
                assert!(diff.multibuffer().is_none());
                assert!(!diff.is_loading());
                assert!(!diff.has_revealed_range(cx));
                assert_eq!(diff.base_text().as_ref(), old_text);
                assert_eq!(diff.file_path(cx), Some(format!("saved-{index}.txt")));
                assert!(!diff.needs_update(&old_text, &new_text, cx));
                assert!(diff.needs_update("changed", &new_text, cx));
                assert!(diff.needs_update(&old_text, "changed", cx));
                assert_eq!(
                    diff.to_markdown(cx),
                    format!("Diff: saved-{index}.txt\n```\n{new_text}\n```\n")
                );
                assert!(diff.buffer().is_none(), "export must not build a diff");
            });
        }

        let diff = diffs.first().expect("saved diff");
        for _ in 0..2 {
            diff.update(cx, |diff, cx| {
                assert!(diff.materialize(cx));
                assert!(!diff.materialize(cx), "opening twice must reuse the view");
            });
            cx.run_until_parked();
            let (buffer, multibuffer) = diff.read_with(cx, |diff, cx| {
                assert!(!diff.is_loading());
                assert!(diff.has_revealed_range(cx));
                let buffer = diff.buffer().expect("opened snapshot buffer");
                assert_eq!(buffer.read(cx).text(), new_text);
                assert_eq!(diff.base_text().as_ref(), old_text);
                (
                    buffer.downgrade(),
                    diff.multibuffer().expect("opened diff").downgrade(),
                )
            });
            for unopened in diffs.iter().skip(1) {
                unopened.read_with(cx, |diff, _| {
                    assert!(diff.buffer().is_none());
                    assert!(diff.multibuffer().is_none());
                });
            }
            diff.update(cx, |diff, cx| {
                assert!(diff.release_render_cache(cx));
                assert!(!diff.release_render_cache(cx));
                assert!(!diff.needs_update(&old_text, &new_text, cx));
            });
            cx.run_until_parked();
            assert!(buffer.upgrade().is_none(), "collapsed buffer retained");
            assert!(multibuffer.upgrade().is_none(), "collapsed diff retained");
        }
    }

    #[gpui::test]
    fn historical_diffs_rebuild_saved_hunks_from_unified_diffs(cx: &mut TestAppContext) {
        let languages = historical_diff_languages(cx);
        let old_text = (1..=40)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let new_text = old_text
            .replace("line 5\n", "line five\n")
            .replace("line 30\n", "line 30\nline 30.5\n");
        let unified_diff = language::unified_diff(&old_text, &new_text);
        let diff = cx.new(|_| {
            Diff::historical_from_unified_diff("saved.txt".into(), &unified_diff, languages)
                .expect("saved diff")
        });
        diff.read_with(cx, |diff, cx| {
            assert!(diff.buffer().is_none());
            assert!(
                diff.base_text().is_empty(),
                "the padded texts are rebuilt only when the edit is opened"
            );

            assert_eq!(
                diff.to_markdown(cx),
                format!(
                    "Diff: saved.txt\n```diff\n{}\n```\n",
                    unified_diff.trim_end()
                )
            );
        });

        diff.update(cx, |diff, cx| assert!(diff.materialize(cx)));
        cx.run_until_parked();
        diff.read_with(cx, |diff, cx| {
            let new_lines = diff.buffer().expect("opened diff").read(cx).text();
            let new_lines = new_lines.lines().collect::<Vec<_>>();
            assert_eq!(new_lines[4], "line five", "hunks keep their rows");
            assert_eq!(new_lines[30], "line 30.5");
            assert_eq!(new_lines[15], "", "rows between hunks are left empty");
            assert_eq!(diff.base_text().lines().nth(4), Some("line 5"));
            let shown = diff
                .multibuffer()
                .expect("opened diff")
                .read(cx)
                .snapshot(cx)
                .text();
            assert!(shown.contains("line five") && shown.contains("line 30.5"));
            assert!(
                !shown.contains("\n\n\n"),
                "distant hunks stay separate excerpts: {shown:?}"
            );
        });
    }

    #[test]
    fn sparse_texts_place_hunks_from_either_diff_convention() {
        assert_eq!(
            sparse_texts_from_unified_diff("").expect("no changes"),
            (String::new(), String::new())
        );
        let created = language::unified_diff("", "first\nsecond\n");
        assert_eq!(
            sparse_texts_from_unified_diff(&created).expect("created file"),
            (String::new(), "first\nsecond\n".to_string())
        );
        assert_eq!(
            sparse_texts_from_unified_diff("@@ -2,0 +3,1 @@\n+inserted\n")
                .expect("insertion without context"),
            ("\n\n".to_string(), "\n\ninserted\n".to_string())
        );
        assert!(
            sparse_texts_from_unified_diff("@@ -4194305,1 +4194305,1 @@\n-a\n+b\n").is_err(),
            "a corrupt header can't pad without bound"
        );
    }

    #[gpui::test(iterations = 5)]
    fn historical_diff_materialization_can_be_canceled_and_reopened(cx: &mut TestAppContext) {
        let languages = historical_diff_languages(cx);
        let diff = cx.new(|_| {
            Diff::historical("created.txt".into(), None, "Complete Ω\n".into(), languages)
        });
        let (canceled_buffer, canceled_multibuffer) = diff.update(cx, |diff, cx| {
            assert!(diff.materialize(cx));
            let buffer = diff.buffer().expect("initial buffer").downgrade();
            let multibuffer = diff.multibuffer().expect("initial diff").downgrade();
            assert!(diff.release_render_cache(cx));
            assert!(diff.materialize(cx));
            (buffer, multibuffer)
        });
        cx.run_until_parked();
        assert!(canceled_buffer.upgrade().is_none());
        assert!(canceled_multibuffer.upgrade().is_none());
        diff.read_with(cx, |diff, cx| {
            assert!(!diff.is_loading());
            assert!(diff.has_revealed_range(cx));
            assert_eq!(
                diff.buffer().expect("reopened buffer").read(cx).text(),
                "Complete Ω\n"
            );
        });
        diff.update(cx, |diff, cx| assert!(diff.release_render_cache(cx)));
        cx.run_until_parked();
        diff.read_with(cx, |diff, _| {
            assert!(diff.buffer().is_none());
            assert!(diff.multibuffer().is_none());
        });
    }

    #[gpui::test]
    fn historical_diff_cache_operations_preserve_live_edits(cx: &mut TestAppContext) {
        historical_diff_languages(cx);
        let buffer = cx.new(|cx| Buffer::local("before\n", cx));
        let diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        diff.update(cx, |diff, cx| {
            assert!(matches!(diff, Diff::Pending(_)));
            assert!(!diff.materialize(cx));
            assert!(!diff.release_render_cache(cx));
            assert_eq!(diff.buffer().expect("live buffer"), &buffer);
        });
        buffer.update(cx, |buffer, cx| buffer.set_text("after\n", cx));
        cx.run_until_parked();
        diff.update(cx, |diff, cx| {
            diff.finalize(cx);
            assert!(matches!(diff, Diff::Finalized(_)));
            assert!(!diff.materialize(cx));
            assert!(!diff.release_render_cache(cx));
        });
        cx.run_until_parked();
        diff.read_with(cx, |diff, cx| {
            assert_eq!(diff.base_text().as_ref(), "before\n");
            assert!(diff.has_revealed_range(cx));
            assert!(diff.buffer().is_some());
        });
    }

    #[gpui::test]
    fn test_finished_live_edits_keep_only_their_hunks(cx: &mut TestAppContext) {
        historical_diff_languages(cx);
        let original = (1..=400)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let edited = original.replace("line 200\n", "line two hundred\n");
        let buffer = cx.new(|cx| Buffer::local(original.clone(), cx));
        let diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| buffer.set_text(edited.clone(), cx));
        cx.run_until_parked();
        diff.update(cx, |diff, cx| diff.finalize(cx));
        cx.run_until_parked();

        diff.read_with(cx, |diff, cx| {
            let shown_buffers = diff
                .multibuffer()
                .expect("finished edit")
                .read(cx)
                .all_buffers();
            assert_eq!(shown_buffers.len(), 1);
            let shown = shown_buffers
                .into_iter()
                .next()
                .expect("shown buffer")
                .read(cx)
                .text();
            assert!(
                shown.len() < original.len() / 4,
                "a finished edit must not keep the whole file: {} bytes",
                shown.len()
            );
            assert_eq!(shown.lines().nth(197), Some("line 198"));
            assert_eq!(shown.lines().nth(199), Some("line two hundred"));
            assert!(diff.to_markdown(cx).contains("+line two hundred"));
            assert_eq!(diff.base_text().as_ref(), original);
            assert_eq!(
                diff.buffer().expect("edited buffer").read(cx).text(),
                edited,
                "discarding an interrupted edit still needs the edited buffer"
            );
        });
    }

    fn source(patch: &str, changes: serde_json::Value) -> acp_v2::Diff {
        serde_json::from_value(json!({
            "changes": changes,
            "patch": {"format": "git_patch", "text": patch}
        }))
        .expect("diff fixture")
    }

    #[test]
    fn test_patch_file_association() {
        let bare = "@@ -10 +10 @@\n-before\n+after\n";
        let single = json!([{"operation": "modify", "path": "/one"}]);
        let parsed = DiffPatch::parse(&source(bare, single.clone())).expect("unique bare hunk");
        assert_eq!(parsed[0].0, 0);
        assert_eq!(parsed[0].1.new_path.as_deref(), Some("/one"));
        assert!(
            DiffPatch::parse(&source(
                bare,
                json!([
                    {"operation": "modify", "path": "/one"},
                    {"operation": "modify", "path": "/two"}
                ])
            ))
            .is_err(),
            "bare hunks must not guess a file"
        );
        let unified = "--- /one\n+++ /one\n@@ -10 +10 @@\n-before\n+after\n";
        assert!(DiffPatch::parse(&source(unified, single)).is_ok());
        assert!(
            DiffPatch::parse(&source(
                unified,
                json!([
                    {"operation": "modify", "path": "/two"}
                ])
            ))
            .is_err(),
            "mismatched paths must fall back"
        );
        assert!(
            DiffPatch::parse(&source(
                unified,
                json!([
                    {"operation": "modify", "path": "/one", "fileType": "binary"}
                ])
            ))
            .is_err(),
            "non-text file must not be rendered as a text diff"
        );
        assert!(
            DiffPatch::parse(&source(
                bare,
                json!([
                    {"operation": "add", "path": "/one"}
                ])
            ))
            .is_err(),
            "added file cannot have deleted content"
        );
        let rename = "--- /old\n+++ /new\n@@ -1 +1 @@\n-before\n+after\n";
        assert!(
            DiffPatch::parse(&source(
                rename,
                json!([
                    {"operation": "move", "oldPath": "/old", "path": "/new"}
                ])
            ))
            .is_ok()
        );
    }

    #[test]
    fn test_patch_file_association_rejects_ambiguity_and_unknown_types() {
        let patch = "diff --git /one /one\n--- /one\n+++ /one\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(
            DiffPatch::parse(&source(
                patch,
                json!([
                    {"operation": "modify", "path": "/one"},
                    {"operation": "modify", "path": "/one"}
                ])
            ))
            .is_err(),
            "duplicate declared paths are ambiguous"
        );
        assert!(
            DiffPatch::parse(&source(
                &patch.repeat(2),
                json!([
                    {"operation": "modify", "path": "/one"}
                ])
            ))
            .is_err(),
            "repeated patch sections must not replace each other"
        );
        for file_type in ["_future", "directory", "symlink"] {
            assert!(
                DiffPatch::parse(&source(
                    patch,
                    json!([
                        {"operation": "modify", "path": "/one", "fileType": file_type}
                    ])
                ))
                .is_err(),
                "{file_type} is not an unspecified file type"
            );
        }
        assert!(
            DiffPatch::parse(&source(
                patch,
                json!([
                    {"operation": "_future", "path": "/one"}
                ])
            ))
            .is_err()
        );
        assert!(
            DiffPatch::parse(&source(
                "--- /old\n+++ /new\n@@ -1 +1 @@\n-before\n+after\n",
                json!([{"operation": "copy", "oldPath": "/old", "path": "/new"}])
            ))
            .is_ok()
        );
    }

    #[test]
    fn test_patch_file_association_preserves_declared_order_for_large_patches() {
        let changes = (0..1_000)
            .map(|index| json!({"operation": "modify", "path": format!("/file-{index}")}))
            .collect::<Vec<_>>();
        let patch = (0..1_000)
            .rev()
            .map(|index| format!(
                "diff --git /file-{index} /file-{index}\n--- /file-{index}\n+++ /file-{index}\n@@ -1 +1 @@\n-old\n+new\n"
            ))
            .collect::<String>();
        let parsed = DiffPatch::parse(&source(&patch, json!(changes))).expect("large patch");
        assert_eq!(parsed.len(), 1_000);
        for ((change_index, file), expected) in parsed.iter().zip((0..1_000).rev()) {
            assert_eq!(*change_index, expected);
            assert_eq!(file.new_path.as_ref(), Some(&format!("/file-{expected}")));
        }
    }

    #[gpui::test]
    async fn test_patch_hunks_render_read_only_snippets(cx: &mut TestAppContext) {
        let patch = indoc! {"
            diff --git /one /one
            --- /one
            +++ /one
            @@ -100,2 +100,2 @@
             context
            -before
            +after
            @@ -900 +900 @@
            -old
            +new
            diff --git /deleted /deleted
            --- /deleted
            +++ /dev/null
            @@ -1 +0,0 @@
            -deleted
            diff --git /added /added
            --- /dev/null
            +++ /added
            @@ -0,0 +1 @@
            +added
        "};
        let source = source(
            patch,
            json!([
                {"operation": "modify", "path": "/one"},
                {"operation": "delete", "path": "/deleted"},
                {"operation": "add", "path": "/added"},
                {"operation": "modify", "path": "/image", "fileType": "binary"},
                {"operation": "move", "oldPath": "/old", "path": "/new"}
            ]),
        );
        let render = cx.update(|cx| {
            let languages = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
            DiffPatch::new(&source, &languages, cx)
        });
        cx.run_until_parked();
        assert!(render.fallback.is_none());
        assert_eq!(
            render.files.len(),
            5,
            "patchless files must remain represented"
        );
        assert_eq!(render.files[0].hunks.len(), 2);
        assert_eq!(
            render.files[0].hunks[0].header.as_ref(),
            "@@ -100,2 +100,2 @@"
        );
        assert_eq!(render.files[0].hunks[1].header.as_ref(), "@@ -900 +900 @@");
        assert!(render.files[3].hunks.is_empty());
        assert!(render.files[4].hunks.is_empty());
        cx.update(|cx| {
            for (file_index, expected) in [
                (0, "context\nbefore\nafter\n"),
                (1, "deleted\n"),
                (2, "added\n"),
            ] {
                let buffer = render.files[file_index].hunks[0].buffer.read(cx);
                assert!(buffer.read_only());
                let snapshot = buffer.snapshot(cx);
                assert_eq!(snapshot.text(), expected);
                assert!(snapshot.diff_hunks().next().is_some());
                for source_buffer in buffer.all_buffers() {
                    assert_eq!(source_buffer.read(cx).capability(), Capability::ReadOnly);
                }
            }
            assert_eq!(
                render.files[0].hunks[1].buffer.read(cx).snapshot(cx).text(),
                "old\nnew\n",
                "sparse hunks must not synthesize the intervening 798 lines"
            );
        });
    }

    #[gpui::test]
    fn test_patch_fallback_retains_file_rows_and_raw_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let languages = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
            let mut source = source(
                "not a patch\n```",
                json!([
                    {"operation": "modify", "path": "/one"}
                ]),
            );
            let render = DiffPatch::new(&source, &languages, cx);
            assert_eq!(render.files.len(), 1);
            assert!(render.files[0].hunks.is_empty());
            let fallback = render.fallback.expect("malformed patch fallback");
            assert!(fallback.read(cx).source().contains("not a patch\n```"));
            source.patch = None;
            let render = DiffPatch::new(&source, &languages, cx);
            assert_eq!(render.files.len(), 1);
            assert!(render.files[0].hunks.is_empty());
            assert!(render.fallback.is_none());
            source.patch = Some(acp_v2::DiffPatch::new("opaque format\n``` nested"));
            source.patch.as_mut().expect("patch").format =
                acp_v2::DiffPatchFormat::Other("_future".into());
            let render = DiffPatch::new(&source, &languages, cx);
            assert!(render.fallback.is_some());
            let content = crate::ToolCallContent::DiffPatch { source, render };
            assert!(
                content
                    .to_markdown(cx)
                    .contains("opaque format\n``` nested"),
                "export must retain supplied text even when its format is unsupported"
            );
        });
    }

    #[gpui::test]
    async fn test_pending_diff(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("hello!", cx));
        let _diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| {
            buffer.set_text("HELLO!", cx);
        });
        cx.run_until_parked();
    }
}
