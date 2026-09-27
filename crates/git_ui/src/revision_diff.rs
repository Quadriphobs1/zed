use anyhow::{Context as _, Result, bail};
use buffer_diff::BufferDiff;
use collections::HashMap;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer,
    SplittableEditor, multibuffer_context_lines,
};
use futures_lite::future::yield_now;
use git::{
    Oid,
    repository::{BranchesScanResult, RevisionDiffMode},
    status::{FileStatus, StatusCode, TrackedStatus},
};
use gpui::{
    App, AppContext as _, AsyncWindowContext, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Subscription, Task, WeakEntity, Window, actions,
};
use language::{BufferId, Capability, LineEnding, OffsetRangeExt as _, Point};
use multi_buffer::PathKey;
use project::{
    Project,
    git_store::{Repository, RevisionDiff},
};
use settings::{DiffViewStyle, Settings};
use std::{any::TypeId, str::FromStr as _, sync::Arc};
use ui::{Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathStyle};
use workspace::{
    Item, Workspace, item::ItemEvent, notifications::NotifyTaskExt as _,
    searchable::SearchableItemHandle,
};

use crate::commit_view::{GitBlob, build_buffer, worktree_id_for_repo_path};

actions!(
    git,
    [
        /// Compare committed HEAD with its locally stored upstream tip, without fetching.
        CompareHeadWithUpstream,
    ]
);

pub(crate) struct RevisionDiffView {
    title: SharedString,
    base: Oid,
    head: Oid,
    mode: RevisionDiffMode,
    editor: Entity<SplittableEditor>,
    state: LoadState,
    _load_task: Task<()>,
    _editor_subscription: Subscription,
}

enum LoadState {
    Loading,
    Loaded { file_count: usize },
    Failed(SharedString),
}

struct RevisionDiffAddon {
    file_statuses: HashMap<BufferId, FileStatus>,
}

impl Addon for RevisionDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(&self, buffer_id: BufferId, _: &App) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }
}

impl RevisionDiffView {
    pub(crate) fn register(workspace: &mut Workspace) {
        workspace.register_action(|workspace, _: &CompareHeadWithUpstream, window, cx| {
            let project = workspace.project().clone();
            let repository = project.read(cx).active_repository(cx);
            let is_local = project.read(cx).is_local();
            let workspace_handle = workspace.weak_handle();
            window
                .spawn(cx, {
                    let workspace_handle = workspace_handle.clone();
                    async move |cx| {
                        anyhow::ensure!(
                            is_local,
                            "Revision diffs are only supported in local projects"
                        );
                        let repository = repository.context("No active Git repository")?;
                        let branches = repository
                            .update(cx, |repository, _| repository.branches())
                            .await??;
                        let (base, head, upstream) = upstream_revisions(branches)?;
                        workspace_handle.update_in(cx, |workspace, window, cx| {
                            Self::open(
                                workspace,
                                repository,
                                base,
                                head,
                                RevisionDiffMode::Direct,
                                format!("HEAD vs {upstream} (local tracking tip)").into(),
                                window,
                                cx,
                            );
                        })?;
                        anyhow::Ok(())
                    }
                })
                .detach_and_notify_err(workspace_handle, window, cx);
        });
    }

    /// Opens pinned local objects without fetching or reading the index/worktree.
    /// PR callers fetch objects separately, then pass the base/head IDs with MergeBase.
    pub(crate) fn open(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        base: Oid,
        head: Oid,
        mode: RevisionDiffMode,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_entity = cx.entity();
        let view = cx.new(|cx| {
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
                multibuffer.set_all_diff_hunks_expanded(cx);
                multibuffer
            });
            let editor = cx.new(|cx| {
                let editor = SplittableEditor::new(
                    EditorSettings::get_global(cx).diff_view_style,
                    multibuffer,
                    project.clone(),
                    workspace_entity,
                    window,
                    cx,
                );
                editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                editor.rhs_editor().update(cx, |editor, cx| {
                    editor.set_show_bookmarks(false, cx);
                    editor.set_show_breakpoints(false, cx);
                });
                editor
            });
            let subscription = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                cx.emit(event.clone());
            });
            let load_task = cx.spawn_in(window, async move |this, cx| {
                let result = async {
                    let diff = repository
                        .update(cx, |repository, _| {
                            repository.load_revision_diff(base, head, mode)
                        })
                        .await
                        .context("Revision diff request was cancelled")??;
                    let file_count = diff.files.len();
                    let base = diff.base;
                    let head = diff.head;
                    load_files(&this, diff, repository, project, cx).await?;
                    anyhow::Ok((file_count, base, head))
                }
                .await;
                this.update(cx, |this, cx| {
                    this.state = match result {
                        Ok((file_count, base, head)) => {
                            this.base = base;
                            this.head = head;
                            LoadState::Loaded { file_count }
                        }
                        Err(error) => {
                            log::error!("Failed to load revision diff: {error:#}");
                            LoadState::Failed(
                                format!("Could not load revision diff: {error:#}").into(),
                            )
                        }
                    };
                    cx.notify();
                })
                .log_err();
            });
            Self {
                title,
                base,
                head,
                mode,
                editor,
                state: LoadState::Loading,
                _load_task: load_task,
                _editor_subscription: subscription,
            }
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }
}

fn upstream_revisions(scan: BranchesScanResult) -> Result<(Oid, Oid, String)> {
    if let Some(error) = scan.error {
        bail!("Could not read Git branches: {error}");
    }
    let branch = scan.branches.iter().find(|branch| branch.is_head).context(
        "HEAD has no attached branch. Check out a branch with a configured upstream first",
    )?;
    let head = branch
        .most_recent_commit
        .as_ref()
        .context("HEAD has no commit to compare")?;
    let upstream = branch.upstream.as_ref().context(
        "This branch has no configured upstream. Configure a remote-tracking upstream first",
    )?;
    let name = upstream
        .ref_name
        .strip_prefix("refs/remotes/")
        .context("The configured upstream is not a remote-tracking ref")?;
    let base = scan.branches.iter().find(|branch| branch.ref_name == upstream.ref_name)
        .and_then(|branch| branch.most_recent_commit.as_ref())
        .context("The upstream tracking tip is not available locally. Fetch separately, then compare again")?;
    Ok((
        Oid::from_str(&base.sha)?,
        Oid::from_str(&head.sha)?,
        name.to_owned(),
    ))
}

async fn load_files(
    view: &WeakEntity<RevisionDiffView>,
    diff: RevisionDiff,
    repository: Entity<Repository>,
    project: Entity<Project>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    let mut file_statuses = HashMap::default();
    for file in diff.files {
        let status = if file.old_text.is_none() {
            StatusCode::Added
        } else if file.new_text.is_none() {
            StatusCode::Deleted
        } else {
            StatusCode::Modified
        };
        let worktree_id = repository
            .read_with(cx, |repository, cx| {
                worktree_id_for_repo_path(repository, project.read(cx), &file.path, cx)
            })
            .context("Project has no worktrees")?;
        let path = PathKey::with_sort_prefix(1, file.path.as_ref().clone());
        let blob = Arc::new(GitBlob {
            display_name: format!(
                "{} — {}",
                diff.head.display_short(),
                file.path.display(PathStyle::local())
            ),
            path: file.path,
            worktree_id,
            is_deleted: file.new_text.is_none(),
            is_binary: file.is_binary,
        });
        let text = if file.is_binary {
            "(binary file changed; contents not shown)".to_owned()
        } else {
            file.new_text.unwrap_or_default()
        };
        let buffer = build_buffer(text, blob, &languages, cx).await?;
        let snapshot = buffer.update(cx, |buffer, cx| {
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer.snapshot()
        });
        file_statuses.insert(
            snapshot.remote_id(),
            FileStatus::Tracked(TrackedStatus {
                index_status: status,
                worktree_status: StatusCode::Unmodified,
            }),
        );
        let buffer_diff = if file.is_binary {
            cx.new(|cx| {
                BufferDiff::new_unchanged(
                    &snapshot,
                    snapshot.language().cloned(),
                    Some(languages.clone()),
                    cx,
                )
            })
        } else {
            let buffer_diff = cx.new(|cx| {
                BufferDiff::new(
                    &snapshot.text,
                    snapshot.language().cloned(),
                    Some(languages.clone()),
                    cx,
                )
            });
            let old_text = file.old_text.map(|mut text| {
                LineEnding::normalize(&mut text);
                Arc::from(text)
            });
            buffer_diff
                .update(cx, |diff, cx| {
                    diff.set_base_text(old_text, snapshot.text.clone(), cx)
                })
                .await;
            buffer_diff
        };
        let ranges = buffer_diff.read_with(cx, |diff, cx| {
            let diff = diff.snapshot(cx);
            let ranges = diff
                .hunks(&snapshot)
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect::<Vec<_>>();
            if ranges.is_empty() {
                vec![Point::zero()..snapshot.max_point()]
            } else {
                ranges
            }
        });
        // Yield between excerpt batches so large immutable comparisons do not monopolize the UI.
        for batch_end in (1..=ranges.len())
            .step_by(10)
            .map(|start| (start + 9).min(ranges.len()))
        {
            view.update_in(cx, |view, window, cx| {
                view.editor.update(cx, |editor, cx| {
                    editor.update_excerpts_for_path(
                        path.clone(),
                        buffer.clone(),
                        ranges[..batch_end].to_vec(),
                        multibuffer_context_lines(cx),
                        buffer_diff.clone(),
                        cx,
                    );
                    if editor.diff_view_style() == DiffViewStyle::Split {
                        editor.split(window, cx);
                    }
                });
            })?;
            yield_now().await;
        }
    }
    view.update(cx, |view, cx| {
        view.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.register_addon(RevisionDiffAddon { file_statuses });
            });
        });
    })?;
    Ok(())
}

impl EventEmitter<EditorEvent> for RevisionDiffView {}

impl Focusable for RevisionDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for RevisionDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone()
    }

    fn to_item_events(event: &EditorEvent, emit: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, emit)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }
}

impl Render for RevisionDiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let base = self.base.to_string();
        let head = self.head.to_string();
        let base_label = match self.mode {
            RevisionDiffMode::Direct => "Base",
            RevisionDiffMode::MergeBase => match self.state {
                LoadState::Loaded { .. } => "Base (merge base)",
                LoadState::Loading => "Base (requested; resolving merge base)",
                LoadState::Failed(_) => "Base (requested)",
            },
        };
        let content = match &self.state {
            LoadState::Loading => Label::new("Loading revision diff…").into_any_element(),
            LoadState::Failed(error) => Label::new(error.clone())
                .color(Color::Error)
                .into_any_element(),
            LoadState::Loaded { file_count: 0 } => {
                Label::new("No changes between these commits.").into_any_element()
            }
            LoadState::Loaded { .. } => div()
                .size_full()
                .child(self.editor.clone())
                .into_any_element(),
        };
        v_flex()
            .key_context("RevisionDiff")
            // Keep the item's focus target rendered while its editor is absent.
            .when(
                !matches!(self.state, LoadState::Loaded { file_count } if file_count > 0),
                |container| container.track_focus(&self.focus_handle(cx)),
            )
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(self.title.clone()))
                    .child(
                        Label::new("Read-only local snapshot • No fetch performed; remote-tracking refs may be stale.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Committed changes only; staged and uncommitted edits are excluded.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("base-sha", format!("{base_label}: {base}"))
                            .tooltip(Tooltip::text("Copy base SHA"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(base.clone()));
                            }),
                    )
                    .child(
                        Button::new("head-sha", format!("Head: {head}"))
                            .tooltip(Tooltip::text("Copy head SHA"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(head.clone()));
                            }),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::{
        Branch, CommitSummary, Upstream, UpstreamTracking, UpstreamTrackingStatus,
    };

    fn branch(name: &str, is_head: bool, sha: &str) -> Branch {
        Branch {
            ref_name: name.to_owned().into(),
            is_head,
            upstream: None,
            most_recent_commit: Some(CommitSummary {
                sha: sha.to_owned().into(),
                subject: "commit".into(),
                commit_timestamp: 0,
                author_name: "Author".into(),
                has_parent: true,
            }),
        }
    }

    fn branches() -> BranchesScanResult {
        let mut head = branch("refs/heads/feature", true, &"1".repeat(40));
        head.upstream = Some(Upstream {
            ref_name: "refs/remotes/origin/feature".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 1,
                behind: 1,
            }),
        });
        vec![
            head,
            branch("refs/remotes/origin/feature", false, &"2".repeat(40)),
        ]
        .into()
    }

    #[gpui::test]
    async fn test_revision_diff_read_only_buffers_and_load_error(cx: &mut gpui::TestAppContext) {
        use project::{FakeFs, git_store::CommitFile};
        use std::path::Path;
        use workspace::MultiWorkspace;

        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(util::path!("/project"), serde_json::json!({".git": {}}))
            .await;
        let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |workspace, _| workspace.workspace().clone())
            .expect("workspace");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        project
            .read_with(&cx, |project, cx| {
                project
                    .worktrees(cx)
                    .next()
                    .expect("worktree")
                    .read(cx)
                    .as_local()
                    .expect("local worktree")
                    .scan_complete()
            })
            .await;
        cx.executor().run_until_parked();
        let repository = project
            .read_with(&cx, |project, cx| project.active_repository(cx))
            .expect("repository");
        let base = Oid::from_str(&"1".repeat(40)).expect("base SHA");
        let head = Oid::from_str(&"2".repeat(40)).expect("head SHA");
        let view = workspace.update_in(&mut cx, |workspace, window, cx| {
            RevisionDiffView::open(
                workspace,
                repository.clone(),
                base,
                head,
                RevisionDiffMode::Direct,
                "Test comparison".into(),
                window,
                cx,
            )
        });
        cx.executor().run_until_parked();
        view.read_with(&cx, |view, cx| {
            assert!(
                matches!(&view.state, LoadState::Failed(error) if error.contains("not supported"))
            );
            assert!(!view.can_save(cx));
            assert!(!view.can_save_as(cx));
        });

        // Placeholder states must retain the item's keyboard dispatch context.
        for state in [
            LoadState::Loading,
            LoadState::Failed("Test failure".into()),
            LoadState::Loaded { file_count: 0 },
        ] {
            view.update_in(&mut cx, |view, window, cx| {
                view.state = state;
                window.focus(&view.focus_handle(cx), cx);
                cx.notify();
            });
            cx.refresh().expect("refresh placeholder");
            cx.run_until_parked();
            cx.update(|window, _| {
                assert!(
                    window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("RevisionDiff")),
                    "placeholder must retain the revision view's focus context"
                );
            });
        }

        let diff = RevisionDiff {
            base,
            head,
            files: vec![
                CommitFile {
                    path: git::repository::repo_path("text.txt"),
                    old_text: Some("before\n".into()),
                    new_text: Some("after\n".into()),
                    is_binary: false,
                },
                CommitFile {
                    path: git::repository::repo_path("binary.dat"),
                    old_text: None,
                    new_text: Some(String::new()),
                    is_binary: true,
                },
                CommitFile {
                    path: git::repository::repo_path("deleted.txt"),
                    old_text: Some("removed\n".into()),
                    new_text: None,
                    is_binary: false,
                },
            ],
        };
        view.update_in(&mut cx, |_, window, cx| {
            let view = cx.weak_entity();
            window.spawn(cx, async move |cx| {
                load_files(&view, diff, repository, project, cx).await
            })
        })
        .await
        .expect("load fixture files");
        let editor = view.read_with(&cx, |view, cx| view.editor.read(cx).rhs_editor().clone());
        editor.update_in(&mut cx, |editor, window, cx| {
            let multibuffer = editor.buffer().clone();
            let before = multibuffer.read(cx).snapshot(cx).text();
            assert_eq!(multibuffer.read(cx).capability(), Capability::ReadOnly);
            let mut buffers = 0;
            multibuffer.read(cx).for_each_buffer(&mut |buffer| {
                buffers += 1;
                assert_eq!(buffer.read(cx).capability(), Capability::ReadOnly);
                assert!(
                    buffer
                        .read(cx)
                        .file()
                        .expect("historic file")
                        .as_local()
                        .is_none()
                );
            });
            assert_eq!(buffers, 3);
            assert!(before.contains("after"));
            assert!(before.contains("binary file changed; contents not shown"));
            editor.insert("must not be inserted", window, cx);
            assert_eq!(multibuffer.read(cx).snapshot(cx).text(), before);
        });
    }

    #[test]
    fn test_revision_diff_upstream_pins_base_and_head() {
        let (base, head, name) = upstream_revisions(branches()).expect("valid upstream");
        assert_eq!(base.to_string(), "2".repeat(40));
        assert_eq!(head.to_string(), "1".repeat(40));
        assert_eq!(name, "origin/feature");
    }

    #[test]
    fn test_revision_diff_upstream_missing_and_local_errors() {
        let mut scan = branches();
        scan.branches[0].upstream = None;
        assert!(
            upstream_revisions(scan)
                .expect_err("missing upstream")
                .to_string()
                .contains("no configured upstream")
        );
        let mut scan = branches();
        scan.branches[0]
            .upstream
            .as_mut()
            .expect("upstream")
            .ref_name = "refs/heads/main".into();
        assert!(
            upstream_revisions(scan)
                .expect_err("local upstream")
                .to_string()
                .contains("not a remote-tracking ref")
        );
        let mut scan = branches();
        scan.branches.pop();
        assert!(
            upstream_revisions(scan)
                .expect_err("missing tracking ref")
                .to_string()
                .contains("Fetch separately")
        );
    }

    #[test]
    fn test_revision_diff_upstream_detached_unborn_and_scan_errors() {
        let mut scan = branches();
        scan.branches[0].is_head = false;
        assert!(
            upstream_revisions(scan)
                .expect_err("detached HEAD")
                .to_string()
                .contains("attached branch")
        );
        let mut scan = branches();
        scan.branches[0].most_recent_commit = None;
        assert!(
            upstream_revisions(scan)
                .expect_err("unborn HEAD")
                .to_string()
                .contains("no commit")
        );
        let mut scan = branches();
        scan.error = Some("partial scan".into());
        assert!(
            upstream_revisions(scan)
                .expect_err("partial scan")
                .to_string()
                .contains("partial scan")
        );
    }
}
