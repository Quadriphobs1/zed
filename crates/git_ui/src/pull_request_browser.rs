use anyhow::{Context as _, Result, ensure};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, Task, WeakEntity,
    Window,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use project::{Project, git_store::Repository, trusted_worktrees::TrustedWorktrees};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{Item, Workspace};

use crate::github_pull_requests::{
    GitHubPullRequestService, GitHubRepository, PullRequestDetail, PullRequestFilter,
    PullRequestSummary, RESULT_LIMIT,
};

enum SelectionDirection {
    Next,
    Previous,
}

#[derive(Default)]
struct ListState {
    generation: usize,
    entries: Vec<PullRequestSummary>,
    loading: bool,
    error: Option<String>,
}

impl ListState {
    fn begin(&mut self) -> usize {
        self.generation += 1;
        self.entries.clear();
        self.error = None;
        self.loading = true;
        self.generation
    }

    fn complete(&mut self, generation: usize, result: Result<Vec<PullRequestSummary>>) {
        if generation != self.generation {
            return;
        }
        self.loading = false;
        match result {
            Ok(entries) => self.entries = entries,
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }
}

pub(super) struct PullRequestBrowser {
    project: Entity<Project>,
    repository: Option<Entity<Repository>>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    remotes: Vec<(String, GitHubRepository)>,
    selected_remote: Option<usize>,
    filter: PullRequestFilter,
    state: ListState,
    selected_entry: Option<usize>,
    scroll_handle: ScrollHandle,
    task: Task<()>,
}

impl PullRequestBrowser {
    pub(super) fn new(
        project: Entity<Project>,
        repository: Option<Entity<Repository>>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            project,
            repository,
            workspace,
            focus_handle: cx.focus_handle(),
            remotes: Vec::new(),
            selected_remote: None,
            filter: PullRequestFilter::All,
            state: ListState::default(),
            selected_entry: None,
            scroll_handle: ScrollHandle::new(),
            task: Task::ready(()),
        };
        this.load_remotes(cx);
        this
    }

    fn load_remotes(&mut self, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        self.remotes.clear();
        self.selected_remote = None;
        self.selected_entry = None;
        let generation = self.state.begin();
        let result = check_access(&self.project, cx)
            .and_then(|()| self.repository.clone().context("No active Git repository"));
        let repository = match result {
            Ok(repository) => repository,
            Err(error) => {
                self.state.complete(generation, Err(error));
                cx.notify();
                return;
            }
        };
        let response = repository.update(cx, |repository, _| repository.remote_urls());
        self.task = cx.spawn(async move |this, cx| {
            let result = response.await.context("Git remote request cancelled").and_then(|result| result);
            this.update(cx, |this, cx| {
                if generation != this.state.generation {
                    return;
                }
                match result {
                    Ok(remotes) => {
                        this.remotes = remotes.into_iter().filter_map(|(name, url)| {
                            Some((name, GitHubRepository::from_remote_url(&url)?))
                        }).collect();
                        this.remotes.sort_by(|left, right| left.0.cmp(&right.0));
                        this.state.complete(generation, Ok(Vec::new()));
                        if this.remotes.is_empty() {
                            this.state.error = Some("No supported github.com remotes. GitHub Enterprise and other hosts are not supported.".into());
                        }
                    }
                    Err(error) => this.state.complete(generation, Err(error)),
                }
                cx.notify();
            }).log_err();
        });
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        self.selected_entry = None;
        self.scroll_handle.set_offset(gpui::point(px(0.), px(0.)));
        let generation = self.state.begin();
        let target = self
            .selected_remote
            .and_then(|index| self.remotes.get(index))
            .map(|(_, target)| target.clone());
        let result =
            check_access(&self.project, cx).and_then(|()| target.context("Select a GitHub remote"));
        let target = match result {
            Ok(target) => target,
            Err(error) => {
                self.state.complete(generation, Err(error));
                cx.notify();
                return;
            }
        };
        let filter = self.filter;
        self.task = cx.spawn(async move |this, cx| {
            let result = GitHubPullRequestService::list(&target, filter).await;
            this.update(cx, |this, cx| {
                this.state.complete(generation, result);
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn open_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.state.entries.get(index) else {
            return;
        };
        let Some((_, target)) = self
            .selected_remote
            .and_then(|index| self.remotes.get(index))
        else {
            return;
        };
        let number = entry.number;
        let target = target.clone();
        let project = self.project.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                let detail = cx.new(|cx| PullRequestDescription::new(project, target, number, cx));
                workspace.add_item_to_active_pane(Box::new(detail), None, true, window, cx);
            })
            .log_err();
    }

    fn select(&mut self, direction: SelectionDirection, cx: &mut Context<Self>) {
        if self.state.entries.is_empty() {
            return;
        }
        let index = match (self.selected_entry, direction) {
            (Some(index), SelectionDirection::Next) => {
                (index + 1).min(self.state.entries.len() - 1)
            }
            (Some(index), SelectionDirection::Previous) => index.saturating_sub(1),
            (None, _) => 0,
        };
        self.selected_entry = Some(index);
        self.scroll_handle.scroll_to_item(index);
        cx.notify();
    }
}

// Check trust before even reading remotes: that operation also invokes a local executable.
fn check_access(project: &Entity<Project>, cx: &mut App) -> Result<()> {
    let project = project.read(cx);
    ensure!(
        project.is_local(),
        "Pull Requests supports local projects only. Remote/SSH projects are not supported."
    );
    let store = project.worktree_store();
    let worktrees: Vec<_> = project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).id())
        .collect();
    let trusted = TrustedWorktrees::try_get_global(cx).context(
        "Workspace trust is unavailable. Trust this workspace before using Pull Requests",
    )?;
    ensure!(
        !worktrees.is_empty()
            && trusted.update(cx, |trusted, cx| worktrees
                .iter()
                .all(|id| trusted.can_trust(&store, *id, cx))),
        "Trust this workspace before using Pull Requests, then reload remotes"
    );
    Ok(())
}

impl Focusable for PullRequestBrowser {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PullRequestBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .min_h_0()
            .gap_2()
            .p_2()
            .track_focus(&self.focus_handle)
            .key_context("PullRequests menu")
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                this.select(SelectionDirection::Next, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                this.select(SelectionDirection::Previous, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                if let Some(index) = this.selected_entry {
                    this.open_entry(index, window, cx);
                }
            }))
            .child(Label::new("GitHub remote (choose explicitly)").size(LabelSize::Small))
            .children(
                self.remotes
                    .iter()
                    .enumerate()
                    .map(|(index, (name, target))| {
                        Button::new(("pr-remote", index), format!("{name} → {}", target.slug()))
                            .toggle_state(self.selected_remote == Some(index))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_remote = Some(index);
                                this.refresh(cx);
                            }))
                    }),
            )
            .child(
                h_flex().flex_wrap().gap_1().children(
                    PullRequestFilter::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(index, filter)| {
                            Button::new(("pr-filter", index), filter.label())
                                .label_size(LabelSize::Small)
                                .toggle_state(self.filter == filter)
                                .disabled(self.selected_remote.is_none())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.filter = filter;
                                    this.refresh(cx);
                                }))
                        }),
                ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("pr-refresh", "Refresh")
                            .disabled(self.selected_remote.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("pr-remotes", "Reload remotes")
                            .on_click(cx.listener(|this, _, _, cx| this.load_remotes(cx))),
                    ),
            )
            .child(
                Label::new(format!(
                    "Open PRs • Up to {RESULT_LIMIT} results; not a complete list"
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.state.loading, |this| {
                this.child(Label::new("Loading…"))
            })
            .when_some(self.state.error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error))
            })
            .when(
                !self.state.loading && self.state.error.is_none() && self.selected_remote.is_none(),
                |this| this.child(Label::new("Select a remote above to load pull requests.")),
            )
            .when(
                !self.state.loading
                    && self.state.error.is_none()
                    && self.selected_remote.is_some()
                    && self.state.entries.is_empty(),
                |this| this.child(Label::new("No open pull requests match this filter.")),
            )
            .child(
                v_flex()
                    .id("pr-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(self.state.entries.iter().enumerate().map(|(index, entry)| {
                        v_flex()
                            .py_1()
                            .child(
                                Button::new(
                                    ("pr-entry", index),
                                    format!("#{} {}", entry.number, entry.title),
                                )
                                .full_width()
                                .toggle_state(self.selected_entry == Some(index))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.selected_entry = Some(index);
                                        this.open_entry(index, window, cx);
                                    },
                                )),
                            )
                            .child(
                                Label::new(format!(
                                    "{}{}",
                                    entry.author_login(),
                                    if entry.is_draft { " • Draft" } else { "" }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                    })),
            )
    }
}

struct PullRequestDescription {
    project: Entity<Project>,
    target: GitHubRepository,
    number: u64,
    focus_handle: FocusHandle,
    detail: Option<PullRequestDetail>,
    markdown: Option<Entity<Markdown>>,
    error: Option<String>,
    loading: bool,
    task: Task<()>,
}

impl PullRequestDescription {
    fn new(
        project: Entity<Project>,
        target: GitHubRepository,
        number: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            project,
            target,
            number,
            focus_handle: cx.focus_handle(),
            detail: None,
            markdown: None,
            error: None,
            loading: false,
            task: Task::ready(()),
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        self.detail = None;
        self.markdown = None;
        self.error = None;
        self.loading = false;
        if let Err(error) = check_access(&self.project, cx) {
            self.error = Some(format!("{error:#}"));
            cx.notify();
            return;
        }
        self.loading = true;
        let target = self.target.clone();
        let number = self.number;
        self.task = cx.spawn(async move |this, cx| {
            let result = GitHubPullRequestService::detail(&target, number).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(detail) => {
                        let body = if detail.body.is_empty() {
                            "No description provided.".into()
                        } else {
                            detail.body.clone()
                        };
                        this.markdown =
                            Some(cx.new(|cx| Markdown::new(body.into(), None, None, cx)));
                        this.detail = Some(detail);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }
}

impl EventEmitter<()> for PullRequestDescription {}
impl Focusable for PullRequestDescription {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Item for PullRequestDescription {
    type Event = ();
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("{} #{}", self.target.slug(), self.number).into()
    }
}
impl Render for PullRequestDescription {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.target.pull_request_url(self.number);
        let diff_url = format!("{url}/files");
        v_flex().id("pr-description").size_full().overflow_y_scroll().p_4().gap_2()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .child(Label::new(format!("{} #{}", self.target.slug(), self.number)))
            .child(h_flex().flex_wrap().gap_2()
                .child(Button::new("pr-detail-refresh", "Refresh description").on_click(cx.listener(|this, _, _, cx| this.refresh(cx))))
                .child(Button::new("pr-browser", "Open on GitHub").on_click(move |_, _, cx| cx.open_url(&url)))
                .child(Button::new("pr-diff-browser", "Open Diff on GitHub").on_click(move |_, _, cx| cx.open_url(&diff_url))))
            .child(Label::new("Native PR diffs are not available yet. Open Diff on GitHub shows the current published changes, not local edits.").color(Color::Muted))
            .when(self.loading, |this| this.child(Label::new("Loading pull request…")))
            .when_some(self.error.clone(), |this, error| this.child(Label::new(error).color(Color::Error)))
            .when_some(self.detail.as_ref(), |this, detail| this
                .child(Label::new(detail.summary.title.clone()))
                .child(Label::new(format!("By {} • {}{}", detail.summary.author_login(), detail.state, if detail.summary.is_draft { " • Draft" } else { "" })))
                .child(Label::new(format!("{} ← {}", detail.base_ref_name, detail.head_ref_name)))
                .child(Label::new(format!("Base: {}", detail.base_ref_oid)).size(LabelSize::Small))
                .child(Label::new(format!("Head: {}", detail.head_ref_oid)).size(LabelSize::Small))
                .child(Label::new("Metadata snapshot; refresh after a force-push. Browser links always show the latest GitHub state.").size(LabelSize::Small).color(Color::Muted)))
            .when_some(self.markdown.clone(), |this, markdown| this.child(MarkdownElement::new(markdown, MarkdownStyle::themed(MarkdownFont::Editor, window, cx))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pull_request_browser_stale_results_and_errors() {
        let mut state = ListState::default();
        let first = state.begin();
        let second = state.begin();
        state.complete(first, Err(anyhow::anyhow!("old repository/filter")));
        assert!(state.loading);
        assert!(state.error.is_none());
        state.complete(second, Err(anyhow::anyhow!("authentication required")));
        assert!(!state.loading);
        assert!(
            state
                .error
                .as_deref()
                .is_some_and(|error| error.contains("authentication required"))
        );
        let third = state.begin();
        assert!(state.error.is_none());
        let entries: Vec<PullRequestSummary> = serde_json::from_value(serde_json::json!([
            {"number": 2, "title": "Current filter", "isDraft": false}
        ]))
        .expect("summaries");
        state.complete(third, Ok(entries));
        assert!(!state.loading);
        assert_eq!(state.entries.first().map(|entry| entry.number), Some(2));
        state.complete(second, Err(anyhow::anyhow!("late error")));
        state.complete(first, Ok(Vec::new()));
        assert!(state.error.is_none());
        assert_eq!(state.entries.first().map(|entry| entry.number), Some(2));
        let fourth = state.begin();
        assert!(state.entries.is_empty());
        state.complete(fourth, Ok(Vec::new()));
        assert!(!state.loading);
        assert!(state.error.is_none());
    }
}
