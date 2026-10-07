use anyhow::Result;
use buffer_diff::BufferDiff;
use gpui::{App, AppContext, AsyncApp, Context, Entity, Subscription, Task};
use itertools::Itertools;
use language::{
    Anchor, Buffer, Capability, LanguageRegistry, OffsetRangeExt as _, Point, TextBuffer,
};
use multi_buffer::{MultiBuffer, PathKey, excerpt_context_lines};
use std::{cmp::Reverse, ops::Range, path::Path, sync::Arc};
use util::ResultExt;

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
            language_registry,
            view: None,
        })
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
        snapshot.view = Some(Self::finalized_data(
            snapshot.path.clone(),
            snapshot
                .base_text_exists
                .then(|| snapshot.base_text.to_string()),
            snapshot.new_text.to_string(),
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
            Self::Historical(HistoricalDiff { base_text, .. }) => base_text,
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
        if let Self::Historical(snapshot) = self {
            return format!("Diff: {}\n```\n{}\n```\n", snapshot.path, snapshot.new_text);
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
            Diff::Historical(snapshot) => {
                snapshot.base_text.as_ref() != old_text || snapshot.new_text.as_ref() != new_text
            }
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

        // Replace the buffer in the multibuffer with the snapshot
        let buffer = cx.new(|cx| {
            let language = self.new_buffer.read(cx).language().cloned();
            let buffer = TextBuffer::new_normalized(
                replica_id,
                cx.entity_id().as_non_zero_u64().into(),
                self.new_buffer.read(cx).line_ending(),
                self.new_buffer.read(cx).as_rope().clone(),
            );
            let mut buffer = Buffer::build(buffer, None, Capability::ReadWrite, cx);
            buffer.set_language(language, cx);
            buffer
        });

        let buffer_diff = cx.spawn({
            let buffer = buffer.clone();
            async move |_this, cx| {
                buffer.update(cx, |buffer, _| buffer.parsing_idle()).await;
                build_buffer_diff(base_text, true, &buffer, cx).await
            }
        });

        let update_diff = cx.spawn(async move |this, cx| {
            let buffer_diff = buffer_diff.await?;
            this.update(cx, |this, cx| {
                let multibuffer = this
                    .multibuffer()
                    .ok_or_else(|| anyhow::anyhow!("Live diff lost its multibuffer"))?;
                multibuffer.update(cx, |multibuffer, cx| {
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
    base_text: Arc<str>,
    base_text_exists: bool,
    new_text: Arc<str>,
    language_registry: Arc<LanguageRegistry>,
    view: Option<FinalizedDiff>,
}

pub struct FinalizedDiff {
    path: String,
    base_text: Arc<str>,
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
    use gpui::{AppContext as _, TestAppContext};
    use language::{Buffer, LanguageRegistry};
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
    async fn test_pending_diff(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("hello!", cx));
        let _diff = cx.new(|cx| Diff::new(buffer.clone(), cx));
        buffer.update(cx, |buffer, cx| {
            buffer.set_text("HELLO!", cx);
        });
        cx.run_until_parked();
    }
}
