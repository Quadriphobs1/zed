use anyhow::{Context as _, Result, ensure};
use collections::HashMap;
use db::kvp::KeyValueStore;
use git::{
    Oid,
    repository::{RepoPath, RevisionDiffMode},
};
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState as GpuiListState, ScrollHandle, Task, WeakEntity, Window, list,
};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownStyle, WrapButtonVisibility,
};
use project::{
    Project,
    git_store::{Repository, RevisionDiff},
    trusted_worktrees::TrustedWorktrees,
};
use settings::Settings;
use std::str::FromStr as _;
use theme_settings::ThemeSettings;
use ui::{
    Chip, ContextMenu, PopoverMenu, TintColor, Tooltip, WithScrollbar, prelude::*,
    right_click_menu, utils::WithRemSize,
};
use util::ResultExt as _;
use workspace::{Item, Workspace};

use crate::change_overview::{ChangeOverview, build_change_overview, narrate_overview};
use crate::commit_file_list::{CommitFileList, render_commit_file};
use crate::commit_file_list::{CommitFileSummary, summarize_commit_files};
use crate::github_pull_requests::{
    ActivityKind, ActivitySection, GitHubPullRequestService, GitHubRepository, PullRequestDetail,
    PullRequestFilter, PullRequestSummary, RESULT_LIMIT,
};
use crate::pull_request_cache::{CachePolicy, PullRequestCache};
use crate::revision_diff::RevisionDiffView;
use std::rc::Rc;

mod activity;
use activity::{ActivityReloaded, PreviewState, PullRequestActivity};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailSection {
    Description,
    Overview,
    Activity,
}

const REMOTE_SELECTION_KEY: &str = "pull-request-browser-remote";
const DESCRIPTION_WIDTH_REMS: f32 = 60.0;
const REFRESH_PR_LABEL: &str = "Refresh pull request";
const PR_FETCH_NOTICE: &str = "Downloads bounded PR history into temporary storage. Never changes checkout history, branches, staged or unstaged edits.";

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RemoteSelection {
    name: String,
    target: String,
}

#[derive(Default)]
struct RemoteSelections {
    entries: collections::HashMap<String, RemoteSelection>,
    save_task: Option<Task<()>>,
}

impl gpui::Global for RemoteSelections {}

impl RemoteSelections {
    fn get(key: &str, cx: &App) -> Option<RemoteSelection> {
        if let Some(selection) = cx
            .try_global::<Self>()
            .and_then(|selections| selections.entries.get(key))
        {
            return Some(selection.clone());
        }
        KeyValueStore::global(cx)
            .scoped(REMOTE_SELECTION_KEY)
            .read(key)
            .log_err()
            .flatten()
            .and_then(|value| serde_json::from_str(&value).log_err())
    }

    fn save(key: String, selection: RemoteSelection, cx: &mut App) {
        let selections = cx.default_global::<Self>();
        let Some(value) = serde_json::to_string(&selection).log_err() else {
            return;
        };
        selections.entries.insert(key.clone(), selection);
        let previous = selections.save_task.take();
        let store = KeyValueStore::global(cx);
        // Share pending choices and write ordering across recreated browsers and windows.
        let task = cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            store
                .scoped(REMOTE_SELECTION_KEY)
                .write(key, value)
                .await
                .log_err();
        });
        cx.default_global::<Self>().save_task = Some(task);
    }
}

impl Drop for RemoteSelections {
    fn drop(&mut self) {
        if let Some(task) = self.save_task.take() {
            task.detach();
        }
    }
}

fn remote_key(path: &std::path::Path) -> String {
    // Display replaces non-Unicode bytes, which can alias distinct repository paths.
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One commit of a pull request, as reported by GitHub.
#[derive(Clone, Debug)]
struct PullRequestCommit {
    oid: Oid,
    title: SharedString,
    author: SharedString,
}

/// The state of a lazily loaded commit list for one pull request.
enum PullRequestCommits {
    Loading,
    Loaded(Rc<[PullRequestCommit]>),
    Error(SharedString),
}

/// A row of the pull request list. Expanded pull requests contribute their
/// commits, and expanded commits contribute their changed files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserRow {
    PullRequest {
        index: usize,
    },
    /// Loading, failure, or "no commits" feedback for an expanded pull request.
    PullRequestMessage {
        index: usize,
    },
    Commit {
        index: usize,
        commit_index: usize,
    },
    /// Loading, failure, or "no files" feedback for an expanded commit.
    CommitMessage {
        index: usize,
        commit_index: usize,
    },
    File {
        index: usize,
        commit_index: usize,
        file_index: usize,
    },
}

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
            Err(error) => {
                self.entries.clear();
                self.error = Some(format!("{error:#}"));
            }
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
    selection_key: Option<String>,
    filter: PullRequestFilter,
    state: ListState,
    selected_entry: Option<usize>,
    /// Pull request rows, their commits and changed files have different
    /// heights, so this list measures each row instead of assuming one height.
    list_state: GpuiListState,
    rows: Rc<[BrowserRow]>,
    task: Task<()>,
    /// Commits of expanded pull requests, keyed by pull request number.
    pull_request_commits: HashMap<u64, PullRequestCommits>,
    pull_request_commit_tasks: HashMap<u64, Task<()>>,
    /// Changed files of expanded commits.
    commit_files: HashMap<Oid, CommitFileList>,
    commit_file_tasks: HashMap<Oid, Task<()>>,
    preview_task: Task<()>,
    preview_error: Option<String>,
}

impl PullRequestBrowser {
    pub(super) fn preload(
        project: Entity<Project>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        if check_access(&project, cx).is_err() {
            return None;
        }
        Some(cx.new(|cx| Self::new(project, Some(repository), workspace, cx)))
    }

    pub(super) fn new(
        project: Entity<Project>,
        repository: Option<Entity<Repository>>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let selection_key = repository
            .as_ref()
            .map(|repository| remote_key(&repository.read(cx).work_directory_abs_path));
        let mut this = Self {
            project,
            repository,
            workspace,
            focus_handle: cx.focus_handle(),
            remotes: Vec::new(),
            selected_remote: None,
            selection_key,
            filter: PullRequestFilter::All,
            state: ListState::default(),
            selected_entry: None,
            list_state: GpuiListState::new(0, ListAlignment::Top, ui::px(400.)),
            rows: Rc::from([]),
            task: Task::ready(()),
            pull_request_commits: HashMap::default(),
            pull_request_commit_tasks: HashMap::default(),
            commit_files: HashMap::default(),
            commit_file_tasks: HashMap::default(),
            preview_task: Task::ready(()),
            preview_error: None,
        };
        this.load_remotes(cx);
        this
    }

    pub(super) fn activate(&mut self, cx: &mut Context<Self>) {
        if self.selected_remote.is_some() {
            self.load_list(CachePolicy::PreferCache, cx);
        }
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
            let result = response
                .await
                .context("Git remote request cancelled")
                .and_then(|result| result);
            this.update(cx, |this, cx| {
                if generation != this.state.generation {
                    return;
                }
                match result {
                    Ok(remotes) => {
                        this.state.complete(generation, Ok(Vec::new()));
                        this.set_remotes(remotes, cx);
                    }
                    Err(error) => this.state.complete(generation, Err(error)),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn set_remotes(
        &mut self,
        remotes: collections::HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        let registry = git::GitHostingProviderRegistry::default_global(cx);
        self.remotes = remotes
            .into_iter()
            .filter_map(|(name, url)| {
                Some((
                    name,
                    GitHubRepository::from_remote_url(registry.clone(), &url)?,
                ))
            })
            .collect();
        self.remotes.sort_by(|left, right| left.0.cmp(&right.0));
        self.selected_remote = None;
        if self.remotes.is_empty() {
            self.state.error = Some("No recognized GitHub remotes. For Enterprise, configure this host as GitHub in git_hosting_providers, then reload remotes. Other providers are not supported.".into());
            return;
        }
        let last_remote = self
            .selection_key
            .as_ref()
            .and_then(|key| RemoteSelections::get(key, cx));
        let index = self
            .remotes
            .iter()
            .position(|(name, target)| {
                last_remote.as_ref().is_some_and(|selection| {
                    selection.name == *name && selection.target == target.target()
                })
            })
            .unwrap_or(0);
        self.select_remote(index, cx);
    }

    fn select_remote(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((name, target)) = self.remotes.get(index) else {
            return;
        };
        let selection = RemoteSelection {
            name: name.clone(),
            target: target.target(),
        };
        self.selected_remote = Some(index);
        if let Some(key) = self.selection_key.clone() {
            RemoteSelections::save(key, selection, cx);
        }
        self.load_list(CachePolicy::PreferCache, cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_list(CachePolicy::Reload, cx);
    }

    fn load_list(&mut self, policy: CachePolicy, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        self.selected_entry = None;
        self.pull_request_commits.clear();
        self.pull_request_commit_tasks.clear();
        self.commit_files.clear();
        self.commit_file_tasks.clear();
        self.preview_error = None;
        self.refresh_rows();
        self.list_state.scroll_to_reveal_item(0);
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
        let scope = self.selection_key.as_deref().unwrap_or_default();
        let (snapshot, response) = PullRequestCache::list(scope, target, self.filter, policy, cx);
        if let Some(snapshot) = snapshot {
            self.state.entries = snapshot;
            self.refresh_rows();
        }
        self.task = cx.spawn(async move |this, cx| {
            let result = response
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()));
            this.update(cx, |this, cx| {
                this.apply_list(generation, result);
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn apply_list(&mut self, generation: usize, result: Result<Vec<PullRequestSummary>>) {
        if generation != self.state.generation {
            return;
        }
        let selected = self
            .selected_entry
            .and_then(|index| self.state.entries.get(index))
            .map(|entry| entry.number);
        self.state.complete(generation, result);
        self.selected_entry = selected.and_then(|number| {
            self.state
                .entries
                .iter()
                .position(|entry| entry.number == number)
        });
        self.refresh_rows();
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
                let repository = self.repository.clone();
                let workspace_handle = cx.weak_entity();
                let detail = cx.new(|cx| {
                    PullRequestDescription::new(
                        project,
                        repository,
                        workspace_handle,
                        target,
                        number,
                        cx,
                    )
                });
                workspace.add_item_to_active_pane(Box::new(detail), None, true, window, cx);
            })
            .log_err();
    }

    fn pull_request_commit(&self, index: usize, commit_index: usize) -> Option<&PullRequestCommit> {
        let entry = self.state.entries.get(index)?;
        let PullRequestCommits::Loaded(commits) = self.pull_request_commits.get(&entry.number)?
        else {
            return None;
        };
        commits.get(commit_index)
    }

    fn render_pull_request_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.state.entries.get(index) else {
            return div().into_any_element();
        };
        let is_expanded = self.pull_request_commits.contains_key(&entry.number);
        let number = entry.number;
        let is_draft = entry.is_draft;
        let title = entry.title.clone();
        let author = entry.author_login().to_owned();

        v_flex()
            .id(("pr-entry", index))
            .debug_selector(move || format!("pr-row-{index}"))
            .cursor_pointer()
            .w_full()
            .min_w_0()
            .py_1()
            .px_2()
            .gap_0p5()
            .border_1()
            .border_color(gpui::transparent_black())
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(self.selected_entry == Some(index), |row| {
                row.border_color(cx.theme().colors().panel_focused_border)
            })
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(
                        IconButton::new(
                            ("pr-disclosure", index),
                            if is_expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text(if is_expanded {
                            "Hide Commits"
                        } else {
                            "Show Commits"
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_pull_request_commits(index, cx);
                        })),
                    )
                    .child(Label::new(title).truncate()),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Label::new(author)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .child(
                        Label::new(format!(
                            "\u{2022} #{number}{}",
                            if is_draft { " \u{2022} Draft" } else { "" }
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .flex_none(),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected_entry = Some(index);
                this.open_entry(index, window, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_commit_row(
        &self,
        index: usize,
        commit_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let commit = self.pull_request_commit(index, commit_index)?;
        let oid = commit.oid;
        let title = commit.title.clone();
        let author = commit.author.clone();
        let short: SharedString = oid.to_string().chars().take(8).collect::<String>().into();
        let is_expanded = self.commit_files.contains_key(&oid);

        Some(
            v_flex()
                .id(("pr-commit", (index * RESULT_LIMIT) + commit_index))
                .cursor_pointer()
                .w_full()
                .min_w_0()
                .py_1()
                .pr_2()
                .pl_4()
                .gap_0p5()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .child(
                            IconButton::new(
                                (
                                    "pr-commit-disclosure",
                                    (index * RESULT_LIMIT) + commit_index,
                                ),
                                if is_expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                },
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(if is_expanded {
                                "Hide Changed Files"
                            } else {
                                "Show Changed Files"
                            }))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_commit_files(oid, cx);
                                },
                            )),
                        )
                        .child(Label::new(title).size(LabelSize::Small).truncate()),
                )
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .pl_4()
                        .gap_1p5()
                        .child(
                            Label::new(author)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                        .child(
                            Label::new(short)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .flex_none(),
                        ),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_commit_preview(oid, None, window, cx);
                }))
                .into_any_element(),
        )
    }

    fn target(&self) -> Option<GitHubRepository> {
        self.selected_remote
            .and_then(|index| self.remotes.get(index))
            .map(|(_, target)| target.clone())
    }

    /// Recomputes the rows of the pull request list, including the commits of
    /// expanded pull requests and the files of expanded commits, and tells the
    /// list which rows changed so unchanged measurements survive.
    fn refresh_rows(&mut self) {
        let rows = browser_rows(
            &self.state.entries,
            &self.pull_request_commits,
            &self.commit_files,
        );
        let old_rows = std::mem::replace(&mut self.rows, rows.clone());
        if *old_rows == *rows {
            return;
        }
        let prefix = old_rows
            .iter()
            .zip(rows.iter())
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = old_rows[prefix..]
            .iter()
            .rev()
            .zip(rows[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        self.list_state.splice(
            prefix..old_rows.len() - suffix,
            rows.len() - prefix - suffix,
        );
    }

    fn row_for_pull_request(&self, index: usize) -> usize {
        self.rows
            .iter()
            .position(|row| matches!(row, BrowserRow::PullRequest { index: row_index } if *row_index == index))
            .unwrap_or(index)
    }

    fn toggle_pull_request_commits(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(number) = self.state.entries.get(index).map(|entry| entry.number) else {
            return;
        };
        if self.pull_request_commits.remove(&number).is_some() {
            self.pull_request_commit_tasks.remove(&number);
            self.refresh_rows();
            cx.notify();
            return;
        }
        let result = check_access(&self.project, cx)
            .and_then(|()| self.target().context("Select a GitHub remote"));
        let target = match result {
            Ok(target) => target,
            Err(error) => {
                self.pull_request_commits.insert(
                    number,
                    PullRequestCommits::Error(format!("{error:#}").into()),
                );
                self.refresh_rows();
                cx.notify();
                return;
            }
        };
        self.pull_request_commits
            .insert(number, PullRequestCommits::Loading);
        self.refresh_rows();
        self.pull_request_commit_tasks.insert(
            number,
            cx.spawn(async move |this, cx| {
                let page = cx
                    .background_spawn(async move {
                        GitHubPullRequestService::activity(
                            &target,
                            number,
                            ActivitySection::Commits,
                            None,
                        )
                        .await
                    })
                    .await;
                this.update(cx, |this, cx| {
                    // The row may have been collapsed, or the list reloaded, while loading.
                    if !this.pull_request_commits.contains_key(&number) {
                        return;
                    }
                    let state = match page {
                        Ok(page) => PullRequestCommits::Loaded(
                            page.items
                                .iter()
                                .filter_map(|item| match &item.kind {
                                    ActivityKind::Commit { oid, title } => {
                                        Some(PullRequestCommit {
                                            oid: *oid,
                                            title: title.clone().into(),
                                            author: item.author.clone().into(),
                                        })
                                    }
                                    _ => None,
                                })
                                .collect(),
                        ),
                        Err(error) => PullRequestCommits::Error(format!("{error:#}").into()),
                    };
                    this.pull_request_commits.insert(number, state);
                    this.refresh_rows();
                    cx.notify();
                })
                .log_err();
            }),
        );
        cx.notify();
    }

    fn toggle_commit_files(&mut self, commit: Oid, cx: &mut Context<Self>) {
        if self.commit_files.remove(&commit).is_some() {
            self.commit_file_tasks.remove(&commit);
            self.refresh_rows();
            cx.notify();
            return;
        }
        let result = check_access(&self.project, cx).and_then(|()| {
            let repository = self
                .repository
                .clone()
                .context("The PR repository is no longer available")?;
            let target = self.target().context("Select a GitHub remote")?;
            anyhow::Ok((repository, target))
        });
        let (repository, target) = match result {
            Ok(value) => value,
            Err(error) => {
                self.commit_files
                    .insert(commit, CommitFileList::Error(format!("{error:#}").into()));
                self.refresh_rows();
                cx.notify();
                return;
            }
        };
        let source = format!("https://{}.git", target.target());
        let response = repository.update(cx, |repository, cx| {
            repository.fetch_pr_commit(source, commit, cx)
        });
        self.commit_files.insert(commit, CommitFileList::Loading);
        self.refresh_rows();
        self.commit_file_tasks.insert(
            commit,
            cx.spawn(async move |this, cx| {
                let summaries = match response.await {
                    Ok(diff) => {
                        let summaries = cx
                            .background_spawn(async move { summarize_commit_files(&diff.files) })
                            .await;
                        Ok(summaries)
                    }
                    Err(error) => Err(error),
                };
                this.update(cx, |this, cx| {
                    if !this.commit_files.contains_key(&commit) {
                        return;
                    }
                    let state = match summaries {
                        Ok(summaries) => CommitFileList::Loaded(summaries.into()),
                        Err(error) => CommitFileList::Error(format!("{error:#}").into()),
                    };
                    this.commit_files.insert(commit, state);
                    this.refresh_rows();
                    cx.notify();
                })
                .log_err();
            }),
        );
        cx.notify();
    }

    /// Opens a read-only preview of one PR commit, optionally loading a single
    /// file first so that it appears at the top of the diff.
    fn open_commit_preview(
        &mut self,
        commit: Oid,
        reveal_file: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = check_access(&self.project, cx).and_then(|()| {
            let repository = self
                .repository
                .clone()
                .context("The PR repository is no longer available")?;
            let target = self.target().context("Select a GitHub remote")?;
            anyhow::Ok((repository, target))
        });
        let (repository, target) = match result {
            Ok(value) => value,
            Err(error) => {
                self.preview_error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let source = format!("https://{}.git", target.target());
        let response = repository.update(cx, |repository, cx| {
            repository.fetch_pr_commit(source, commit, cx)
        });
        let title: SharedString = format!("{} commit {commit}", target.slug()).into();
        let workspace = self.workspace.clone();
        self.preview_error = None;
        self.preview_task = cx.spawn_in(window, async move |this, cx| {
            let result = response.await;
            let result = cx.update(|window, cx| {
                let mut snapshot = result?;
                if let Some(reveal_file) = &reveal_file
                    && let Some(index) = snapshot
                        .files
                        .iter()
                        .position(|file| &file.path == reveal_file)
                {
                    let file = snapshot.files.remove(index);
                    snapshot.files.insert(0, file);
                }
                workspace.update(cx, |workspace, cx| {
                    RevisionDiffView::open_snapshot(
                        workspace,
                        repository,
                        snapshot,
                        RevisionDiffMode::Direct,
                        title,
                        window,
                        cx,
                    );
                })
            });
            this.update(cx, |this, cx| {
                this.preview_error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) | Err(error) => Some(format!("{error:#}")),
                };
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
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
        self.list_state
            .scroll_to_reveal_item(self.row_for_pull_request(index));
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
            .overflow_hidden()
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
            .child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .child(Label::new("GitHub remote").size(LabelSize::Small))
                    .children(
                        self.remotes
                            .iter()
                            .enumerate()
                            .map(|(index, (name, target))| {
                                Button::new(
                                    ("pr-remote", index),
                                    format!("{name} → {}", target.target()),
                                )
                                .toggle_state(self.selected_remote == Some(index))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.select_remote(index, cx);
                                    },
                                ))
                            }),
                    )
                    .child(
                        h_flex().flex_wrap().gap_1().children(
                            PullRequestFilter::ALL.into_iter().enumerate().map(
                                |(index, filter)| {
                                    Button::new(("pr-filter", index), filter.label())
                                        .label_size(LabelSize::Small)
                                        .toggle_state(self.filter == filter)
                                        .disabled(self.selected_remote.is_none())
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.filter = filter;
                                            this.load_list(CachePolicy::PreferCache, cx);
                                        }))
                                },
                            ),
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
                        this.child(Label::new(if self.state.entries.is_empty() {
                            "Loading…"
                        } else {
                            "Updating…"
                        }))
                    })
                    .when_some(self.state.error.clone(), |this, error| {
                        this.child(Label::new(error).color(Color::Error))
                    })
                    .when(
                        !self.state.loading
                            && self.state.error.is_none()
                            && self.selected_remote.is_none(),
                        |this| {
                            this.child(Label::new("Select a remote above to load pull requests."))
                        },
                    )
                    .when(
                        !self.state.loading
                            && self.state.error.is_none()
                            && self.selected_remote.is_some()
                            && self.state.entries.is_empty(),
                        |this| this.child(Label::new("No open pull requests match this filter.")),
                    ),
            )
            .when_some(self.preview_error.clone(), |this, error| {
                this.child(
                    div()
                        .px_2()
                        .child(Label::new(error).color(Color::Error).size(LabelSize::Small)),
                )
            })
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, row_index: usize, _, cx| {
                        let rows = this.rows.clone();
                        rows.get(row_index)
                            .into_iter()
                            .filter_map(|row| match *row {
                                BrowserRow::PullRequest { index } => {
                                    Some(this.render_pull_request_row(index, cx))
                                }
                                BrowserRow::PullRequestMessage { index } => {
                                    let entry = this.state.entries.get(index)?;
                                    let message =
                                        match this.pull_request_commits.get(&entry.number)? {
                                            PullRequestCommits::Loading => {
                                                SharedString::new_static("Loading commits\u{2026}")
                                            }
                                            PullRequestCommits::Error(error) => error.clone(),
                                            PullRequestCommits::Loaded(_) => {
                                                SharedString::new_static("No commits")
                                            }
                                        };
                                    Some(render_list_message(message, 4.))
                                }
                                BrowserRow::Commit {
                                    index,
                                    commit_index,
                                } => this.render_commit_row(index, commit_index, cx),
                                BrowserRow::CommitMessage {
                                    index,
                                    commit_index,
                                } => {
                                    let commit = this.pull_request_commit(index, commit_index)?;
                                    let message = match this.commit_files.get(&commit.oid)? {
                                        CommitFileList::Loading => SharedString::new_static(
                                            "Loading changed files\u{2026}",
                                        ),
                                        CommitFileList::Error(error) => error.clone(),
                                        CommitFileList::Loaded(_) => {
                                            SharedString::new_static("No files changed")
                                        }
                                    };
                                    Some(render_list_message(message, 6.))
                                }
                                BrowserRow::File {
                                    index,
                                    commit_index,
                                    file_index,
                                } => {
                                    let commit = this.pull_request_commit(index, commit_index)?;
                                    let oid = commit.oid;
                                    let CommitFileList::Loaded(files) =
                                        this.commit_files.get(&oid)?
                                    else {
                                        return None;
                                    };
                                    let file = files.get(file_index)?;
                                    let path = file.path.clone();
                                    Some(render_commit_file(
                                        ("pr-commit-file", row_index),
                                        file,
                                        cx.listener(move |this, _, window, cx| {
                                            this.open_commit_preview(
                                                oid,
                                                Some(path.clone()),
                                                window,
                                                cx,
                                            );
                                        }),
                                        cx,
                                    ))
                                }
                            })
                            .next()
                            .unwrap_or_else(|| div().into_any_element())
                    }),
                )
                .flex_1()
                .size_full(),
            )
    }
}

fn browser_rows(
    entries: &[PullRequestSummary],
    pull_request_commits: &HashMap<u64, PullRequestCommits>,
    commit_files: &HashMap<Oid, CommitFileList>,
) -> Rc<[BrowserRow]> {
    let mut rows = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        rows.push(BrowserRow::PullRequest { index });
        let Some(commits) = pull_request_commits.get(&entry.number) else {
            continue;
        };
        let PullRequestCommits::Loaded(commits) = commits else {
            rows.push(BrowserRow::PullRequestMessage { index });
            continue;
        };
        if commits.is_empty() {
            rows.push(BrowserRow::PullRequestMessage { index });
            continue;
        }
        for (commit_index, commit) in commits.iter().enumerate() {
            rows.push(BrowserRow::Commit {
                index,
                commit_index,
            });
            let Some(files) = commit_files.get(&commit.oid) else {
                continue;
            };
            let CommitFileList::Loaded(files) = files else {
                rows.push(BrowserRow::CommitMessage {
                    index,
                    commit_index,
                });
                continue;
            };
            if files.is_empty() {
                rows.push(BrowserRow::CommitMessage {
                    index,
                    commit_index,
                });
                continue;
            }
            rows.extend((0..files.len()).map(|file_index| BrowserRow::File {
                index,
                commit_index,
                file_index,
            }));
        }
    }
    rows.into()
}

fn render_list_message(message: SharedString, indent: f32) -> AnyElement {
    h_flex()
        .w_full()
        .min_w_0()
        .h_full()
        .items_center()
        .pr_2()
        .pl(ui::rems(indent / 4.))
        .child(
            Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .into_any_element()
}

struct PreviewRequest {
    generation: usize,
    mode: RevisionDiffMode,
    repository: Entity<Repository>,
    title: SharedString,
    /// Loaded first so that it appears at the top of the diff view.
    reveal_file: Option<RepoPath>,
}

struct PullRequestDescription {
    project: Entity<Project>,
    repository: Option<Entity<Repository>>,
    workspace: WeakEntity<Workspace>,
    scope: String,
    target: GitHubRepository,
    number: u64,
    focus_handle: FocusHandle,
    detail: Option<PullRequestDetail>,
    markdown: Option<Entity<Markdown>>,
    error: Option<String>,
    loading: bool,
    task: Task<()>,
    diff_loading: bool,
    diff_error: Option<String>,
    diff_task: Task<()>,
    preview_generation: usize,
    details_expanded: bool,
    section: DetailSection,
    overview: Option<Entity<Markdown>>,
    overview_narration: Option<Entity<Markdown>>,
    overview_summary: Option<ChangeOverview>,
    overview_error: Option<String>,
    overview_loading: bool,
    overview_narrating: bool,
    overview_task: Task<()>,
    activity: Option<Entity<PullRequestActivity>>,
    activity_subscription: Option<gpui::Subscription>,
    scroll_handle: ScrollHandle,
}

impl PullRequestDescription {
    fn new(
        project: Entity<Project>,
        repository: Option<Entity<Repository>>,
        workspace: WeakEntity<Workspace>,
        target: GitHubRepository,
        number: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        let scope = repository
            .as_ref()
            .map(|repository| remote_key(&repository.read(cx).work_directory_abs_path))
            .unwrap_or_default();
        let mut this = Self {
            project,
            repository,
            workspace,
            scope,
            target,
            number,
            focus_handle: cx.focus_handle(),
            detail: None,
            markdown: None,
            error: None,
            loading: false,
            task: Task::ready(()),
            diff_loading: false,
            diff_error: None,
            diff_task: Task::ready(()),
            preview_generation: 0,
            details_expanded: false,
            section: DetailSection::Description,
            overview: None,
            overview_narration: None,
            overview_summary: None,
            overview_error: None,
            overview_loading: false,
            overview_narrating: false,
            overview_task: Task::ready(()),
            activity: None,
            activity_subscription: None,
            scroll_handle: ScrollHandle::new(),
        };
        this.load_detail(CachePolicy::PreferCache, cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_detail(CachePolicy::Reload, cx);
        if let Some(activity) = &self.activity {
            activity.update(cx, |activity, cx| activity.refresh(cx));
        }
    }

    pub(super) fn show_section(&mut self, section: DetailSection, cx: &mut Context<Self>) {
        self.section = section;
        self.scroll_handle.set_offset(gpui::Point::default());
        if self.section == DetailSection::Activity && self.activity.is_none() {
            let description = cx.weak_entity();
            let on_commit = std::rc::Rc::new({
                let description = description.clone();
                move |commit, reveal_file, window: &mut Window, cx: &mut App| {
                    description
                        .update(cx, |this, cx| {
                            this.open_commit(commit, reveal_file, window, cx)
                        })
                        .log_err();
                }
            });
            let load_commit_files = std::rc::Rc::new(move |commit, cx: &mut App| {
                description
                    .update(cx, |this, cx| this.load_commit_files(commit, cx))
                    .unwrap_or_else(|error| Task::ready(Err(error)))
            });
            let activity = cx.new(|cx| {
                PullRequestActivity::new(
                    self.project.clone(),
                    self.target.clone(),
                    self.number,
                    self.scroll_handle.clone(),
                    on_commit,
                    load_commit_files,
                    cx,
                )
            });
            self.activity_subscription = Some(cx.subscribe(
                &activity,
                |this, _, _: &ActivityReloaded, cx| {
                    // The child resets its busy state; do not update a potentially leased emitter.
                    this.cancel_diff();
                    cx.notify();
                },
            ));
            activity.update(cx, |activity, cx| {
                activity.set_preview(
                    if self.diff_loading {
                        PreviewState::Loading
                    } else {
                        PreviewState::Ready
                    },
                    cx,
                );
                activity.refresh(cx);
            });
            self.activity = Some(activity);
        }
        cx.notify();
    }

    /// Fetches the files a PR commit touches without opening a diff view.
    fn load_commit_files(
        &mut self,
        commit: Oid,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<CommitFileSummary>>> {
        let result = check_access(&self.project, cx).and_then(|()| {
            self.repository
                .clone()
                .context("The PR repository is no longer available")
        });
        let repository = match result {
            Ok(repository) => repository,
            Err(error) => return Task::ready(Err(error)),
        };
        let source = format!("https://{}.git", self.target.target());
        let response = repository.update(cx, |repository, cx| {
            repository.fetch_pr_commit(source, commit, cx)
        });
        cx.background_spawn(async move {
            let diff = response.await?;
            Ok(summarize_commit_files(&diff.files))
        })
    }

    fn open_commit(
        &mut self,
        commit: Oid,
        reveal_file: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.diff_loading {
            return;
        }
        let result = check_access(&self.project, cx).and_then(|()| {
            self.repository
                .clone()
                .context("The PR repository is no longer available")
        });
        let repository = match result {
            Ok(repository) => repository,
            Err(error) => {
                self.diff_error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let source = format!("https://{}.git", self.target.target());
        let response = repository.update(cx, |repository, cx| {
            repository.fetch_pr_commit(source, commit, cx)
        });
        let title = format!("{} #{} commit {}", self.target.slug(), self.number, commit).into();
        self.show_diff(
            response,
            repository,
            title,
            RevisionDiffMode::Direct,
            reveal_file,
            window,
            cx,
        );
    }

    fn cancel_diff(&mut self) {
        self.preview_generation += 1;
        self.diff_task = Task::ready(());
        self.diff_loading = false;
        self.diff_error = None;
    }

    fn show_diff(
        &mut self,
        response: Task<Result<RevisionDiff>>,
        repository: Entity<Repository>,
        title: SharedString,
        mode: RevisionDiffMode,
        reveal_file: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview_generation += 1;
        let request = PreviewRequest {
            generation: self.preview_generation,
            mode,
            repository,
            title,
            reveal_file,
        };
        self.diff_error = None;
        self.diff_loading = true;
        if let Some(activity) = &self.activity {
            activity.update(cx, |activity, cx| {
                activity.set_preview(PreviewState::Loading, cx)
            });
        }
        self.diff_task = cx.spawn_in(window, async move |this, cx| {
            let result = response.await;
            cx.update(|window, cx| {
                Self::finish_diff(&this, result, request, window, cx);
            })
            .log_err();
        });
        cx.notify();
    }

    fn set_detail(&mut self, detail: PullRequestDetail, cx: &mut Context<Self>) {
        let body = if detail.body.is_empty() {
            "No description provided.".into()
        } else {
            detail.body.clone()
        };
        let languages = self.project.read(cx).languages().clone();
        self.markdown = Some(cx.new(|cx| Markdown::new(body.into(), Some(languages), None, cx)));
        self.detail = Some(detail);
    }

    fn open_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            check_access(&self.project, cx)?;
            let detail = self
                .detail
                .as_ref()
                .context("Load the PR description before previewing changes")?;
            let repository = self
                .repository
                .clone()
                .context("The PR repository is no longer available")?;
            let base = Oid::from_str(&detail.base_ref_oid).context("Invalid PR base revision")?;
            let head = Oid::from_str(&detail.head_ref_oid).context("Invalid PR head revision")?;
            anyhow::Ok((repository, base, head))
        })();
        let (repository, base, head) = match result {
            Ok(revisions) => revisions,
            Err(error) => {
                self.diff_error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let source = format!("https://{}.git", self.target.target());
        let head_ref = format!("refs/pull/{}/head", self.number);
        let response = repository.update(cx, |repository, cx| {
            repository.fetch_pr_objects(source, base, head, head_ref, cx)
        });
        let title: SharedString = format!("{} #{} changes", self.target.slug(), self.number).into();
        self.show_diff(
            response,
            repository,
            title,
            RevisionDiffMode::MergeBase,
            None,
            window,
            cx,
        );
    }

    fn finish_diff(
        this: &WeakEntity<Self>,
        result: Result<RevisionDiff>,
        request: PreviewRequest,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((project, workspace)) = this
            .read_with(cx, |this, _| {
                (this.preview_generation == request.generation)
                    .then(|| (this.project.clone(), this.workspace.clone()))
            })
            .log_err()
            .flatten()
        else {
            return;
        };
        let result = (|| {
            let mut snapshot = result?;
            check_access(&project, cx)?;
            if let Some(reveal_file) = &request.reveal_file
                && let Some(index) = snapshot
                    .files
                    .iter()
                    .position(|file| &file.path == reveal_file)
            {
                let file = snapshot.files.remove(index);
                snapshot.files.insert(0, file);
            }

            // Adding a pane item reads existing items; do not hold the description's lease.
            workspace.update(cx, |workspace, cx| {
                RevisionDiffView::open_snapshot(
                    workspace,
                    request.repository,
                    snapshot,
                    request.mode,
                    request.title,
                    window,
                    cx,
                );
            })
        })();
        this.update(cx, |this, cx| {
            this.diff_loading = false;
            if let Some(activity) = &this.activity {
                activity.update(cx, |activity, cx| {
                    activity.set_preview(PreviewState::Ready, cx)
                });
            }
            this.diff_error = result.err().map(|error| format!("{error:#}"));
            cx.notify();
        })
        .log_err();
    }

    /// Builds a structural, code-free picture of the pull request. The diff is
    /// fetched the same way the preview fetches it, but nothing is opened.
    fn build_overview(&mut self, cx: &mut Context<Self>) {
        if self.overview_loading {
            return;
        }
        let result = (|| {
            check_access(&self.project, cx)?;
            let detail = self
                .detail
                .as_ref()
                .context("Load the pull request before building an overview")?;
            let repository = self
                .repository
                .clone()
                .context("The PR repository is no longer available")?;
            let base = Oid::from_str(&detail.base_ref_oid).context("Invalid PR base revision")?;
            let head = Oid::from_str(&detail.head_ref_oid).context("Invalid PR head revision")?;
            anyhow::Ok((repository, base, head))
        })();
        let (repository, base, head) = match result {
            Ok(revisions) => revisions,
            Err(error) => {
                self.overview_error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };

        let source = format!("https://{}.git", self.target.target());
        let head_ref = format!("refs/pull/{}/head", self.number);
        let diff = repository.update(cx, |repository, cx| {
            repository.fetch_pr_objects(source.clone(), base, head, head_ref, cx)
        });
        let commits = cx.background_spawn({
            let target = self.target.clone();
            let number = self.number;
            async move {
                GitHubPullRequestService::activity(&target, number, ActivitySection::Commits, None)
                    .await
            }
        });
        let title: SharedString = format!("{} #{}", self.target.slug(), self.number).into();
        let languages = self.project.read(cx).languages().clone();

        self.overview_error = None;
        self.overview_loading = true;
        self.overview_narration = None;
        self.overview_task = cx.spawn(async move |this, cx| {
            let diff = diff.await;
            // Commits are decoration: a failure there must not lose the diff.
            let commits = commits
                .await
                .map(|page| {
                    page.items
                        .iter()
                        .filter_map(|item| match &item.kind {
                            ActivityKind::Commit { oid, title } => Some((
                                oid.to_string().chars().take(8).collect::<String>().into(),
                                SharedString::from(title.clone()),
                            )),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let overview = match diff {
                Ok(diff) => {
                    cx.update(|cx| {
                        build_change_overview(title, diff.files, commits, Some(languages), cx)
                    })
                    .await
                }
                Err(error) => Err(error),
            };

            this.update(cx, |this, cx| {
                this.overview_loading = false;
                match overview {
                    Ok(overview) => {
                        let markdown = overview.to_markdown();
                        this.overview = Some(cx.new(|cx| {
                            Markdown::new_with_options(
                                markdown.into(),
                                Some(this.project.read(cx).languages().clone()),
                                None,
                                markdown::MarkdownOptions {
                                    render_mermaid_diagrams: true,
                                    ..Default::default()
                                },
                                cx,
                            )
                        }));
                        this.overview_summary = Some(overview);
                    }
                    Err(error) => this.overview_error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    /// Asks a configured model to narrate the structure that was computed
    /// locally. The model never sees the diff itself.
    fn narrate_overview(&mut self, cx: &mut Context<Self>) {
        if self.overview_narrating {
            return;
        }
        let Some(overview) = self.overview_summary.clone() else {
            self.overview_error = Some("Build the overview before narrating it".into());
            cx.notify();
            return;
        };
        let narration = narrate_overview(&overview, cx);
        self.overview_error = None;
        self.overview_narrating = true;
        self.overview_task = cx.spawn(async move |this, cx| {
            let narration = narration.await;
            this.update(cx, |this, cx| {
                this.overview_narrating = false;
                match narration {
                    Ok(narration) => {
                        this.overview_narration = Some(cx.new(|cx| {
                            Markdown::new_with_options(
                                narration.into(),
                                Some(this.project.read(cx).languages().clone()),
                                None,
                                markdown::MarkdownOptions {
                                    render_mermaid_diagrams: true,
                                    ..Default::default()
                                },
                                cx,
                            )
                        }));
                    }
                    Err(error) => this.overview_error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn load_detail(&mut self, policy: CachePolicy, cx: &mut Context<Self>) {
        self.cancel_diff();
        if let Some(activity) = &self.activity {
            activity.update(cx, |activity, cx| {
                activity.set_preview(PreviewState::Ready, cx)
            });
        }
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
        let (snapshot, response) =
            PullRequestCache::detail(&self.scope, self.target.clone(), self.number, policy, cx);
        if let Some(snapshot) = snapshot {
            self.set_detail(snapshot, cx);
        }
        self.task = cx.spawn(async move |this, cx| {
            let result = response
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()));
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(detail) => this.set_detail(detail, cx),
                    Err(error) => {
                        this.detail = None;
                        this.markdown = None;
                        this.error = Some(format!("{error:#}"));
                    }
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
impl PullRequestDescription {
    fn render_header(&self, cx: &Context<Self>) -> AnyElement {
        let url = self.target.pull_request_url(self.number);
        v_flex()
            .debug_selector(|| "pr-header".into())
            .flex_none()
            .w_full()
            .min_w_0()
            .p_4()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Label::new(format!("{} #{}", self.target.slug(), self.number))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate()
                            .flex_1(),
                    )
                    .when_some(self.detail.as_ref(), |this, detail| {
                        let (label, color) = match (detail.state.as_str(), detail.summary.is_draft)
                        {
                            ("OPEN", true) => ("Draft", Color::Muted),
                            ("OPEN", false) => ("Open", Color::Success),
                            ("MERGED", _) => ("Merged", Color::Accent),
                            ("CLOSED", _) => ("Closed", Color::Muted),
                            _ => (detail.state.as_str(), Color::Muted),
                        };
                        this.child(
                            Chip::new(label.to_owned())
                                .icon(IconName::PullRequest)
                                .label_color(color)
                                .icon_color(color),
                        )
                    }),
            )
            .when_some(self.detail.as_ref(), |this, detail| {
                this.child(
                    div()
                        .id("pr-title")
                        .debug_selector(|| "pr-title".into())
                        .w_full()
                        .min_w_0()
                        .tooltip(Tooltip::text(detail.summary.title.clone()))
                        .child(
                            Label::new(detail.summary.title.clone())
                                .size(LabelSize::Large)
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .line_clamp(3),
                        ),
                )
                .child(
                    Label::new(format!("By {}", detail.summary.author_login()))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .child(
                            h_flex()
                                .id("pr-head-branch")
                                .debug_selector(|| "pr-head-branch".into())
                                .min_w_0()
                                .flex_1()
                                .child(
                                    Chip::new(detail.head_ref_name.clone())
                                        .icon(IconName::GitBranch)
                                        .truncate()
                                        .tooltip(Tooltip::text(detail.head_ref_name.clone())),
                                ),
                        )
                        .child(
                            Icon::new(IconName::ArrowRight)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            h_flex()
                                .id("pr-base-branch")
                                .debug_selector(|| "pr-base-branch".into())
                                .min_w_0()
                                .max_w(relative(0.5))
                                .child(
                                    Chip::new(detail.base_ref_name.clone())
                                        .icon(IconName::GitBranch)
                                        .truncate()
                                        .tooltip(Tooltip::text(detail.base_ref_name.clone())),
                                ),
                        ),
                )
            })
            .child(
                h_flex()
                    .debug_selector(|| "pr-actions".into())
                    .w_full()
                    .min_w_0()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        IconButton::new("pr-detail-refresh", IconName::RefreshTitle)
                            .aria_label(REFRESH_PR_LABEL)
                            .tab_index(0_isize)
                            .tooltip(Tooltip::text(REFRESH_PR_LABEL))
                            .disabled(self.loading)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        div()
                            .debug_selector(|| "pr-native-control".into())
                            .flex_none()
                            .child(
                                Button::new("pr-diff-native", "View changes")
                                    .tab_index(0_isize)
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .tooltip(Tooltip::text(PR_FETCH_NOTICE))
                                    .aria_description(PR_FETCH_NOTICE)
                                    .disabled(self.detail.is_none() || self.diff_loading)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_diff(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "pr-github-control".into())
                            .flex_none()
                            .child(
                                PopoverMenu::new("pr-github-menu")
                                    .trigger(
                                        Button::new("pr-github", "GitHub")
                                            .tab_index(0_isize)
                                            .end_icon(
                                                Icon::new(IconName::ChevronDown)
                                                    .size(IconSize::Small),
                                            ),
                                    )
                                    .menu(move |window, cx| {
                                        let url = url.clone();
                                        let diff_url = format!("{url}/files");
                                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                            menu.entry(
                                                "Open pull request on GitHub",
                                                None,
                                                move |_, cx| cx.open_url(&url),
                                            )
                                            .entry(
                                                "Open diff on GitHub",
                                                None,
                                                move |_, cx| cx.open_url(&diff_url),
                                            )
                                        }))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "pr-details-toggle".into())
                            .flex_none()
                            .child(
                                Button::new("pr-details", "Details")
                                    .tab_index(0_isize)
                                    .aria_expanded(self.details_expanded)
                                    .end_icon(
                                        Icon::new(if self.details_expanded {
                                            IconName::ChevronUp
                                        } else {
                                            IconName::ChevronDown
                                        })
                                        .size(IconSize::Small),
                                    )
                                    .toggle_state(self.details_expanded)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.details_expanded = !this.details_expanded;
                                        if this.details_expanded {
                                            // Reveal the expanded metadata even after scrolling a long description.
                                            this.scroll_handle.set_offset(gpui::Point::default());
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .debug_selector(|| "pr-activity-toggle".into())
                            .flex_none()
                            .gap_1()
                            .children(
                                [
                                    (DetailSection::Description, "Description"),
                                    (DetailSection::Overview, "Overview"),
                                    (DetailSection::Activity, "Activity"),
                                ]
                                .into_iter()
                                .map(|(section, label)| {
                                    div()
                                        .debug_selector(move || format!("pr-section-{label}"))
                                        .child(
                                            Button::new(label, label)
                                                .tab_index(0_isize)
                                                .toggle_state(self.section == section)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.show_section(section, cx)
                                                })),
                                        )
                                }),
                            ),
                    ),
            )
            .when(self.loading, |this| {
                this.child(
                    Label::new(if self.detail.is_some() {
                        "Updating pull request…"
                    } else {
                        "Loading pull request…"
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .when(self.diff_loading, |this| {
                this.child(
                    Label::new("Preparing changes…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(
                [self.error.as_ref(), self.diff_error.as_ref()]
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(index, error)| {
                        div()
                            .id(("pr-error", index))
                            .w_full()
                            .min_w_0()
                            .tooltip(Tooltip::text(error.clone()))
                            .child(Label::new(error.clone()).color(Color::Error).line_clamp(3))
                    }),
            )
            .into_any_element()
    }

    fn render_details(&self, cx: &Context<Self>) -> AnyElement {
        v_flex().debug_selector(|| "pr-revision-details".into())
            .w_full().min_w_0().p_3().gap_2().rounded_md()
            .bg(cx.theme().colors().element_background)
            .border_1().border_color(cx.theme().colors().border)
            .when_some(self.detail.as_ref(), |this, detail| this.children(
                [("Base", &detail.base_ref_oid), ("Head", &detail.head_ref_oid)].into_iter().map(|(label, oid)| {
                    let oid = oid.clone();
                    let copy = oid.clone();
                    let copy_label = format!("Copy {label} commit ID");
                    h_flex().w_full().min_w_0().gap_2()
                        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                        .child(div().id(format!("pr-revision-{label}")).flex_1().min_w_0().tooltip(Tooltip::text(oid.clone()))
                            .child(Label::new(oid).size(LabelSize::Small).truncate()))
                        .child(IconButton::new(format!("pr-copy-{label}"), IconName::Copy)
                            .tab_index(0_isize)
                            .aria_label(copy_label.clone())
                            .tooltip(Tooltip::text(copy_label))
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))))
                })))
            .child(Label::new("Metadata snapshot; refresh after a force-push. GitHub links show the latest published state.")
                .size(LabelSize::Small).color(Color::Muted))
            .child(Label::new(PR_FETCH_NOTICE).size(LabelSize::Small).color(Color::Muted))
            .into_any_element()
    }

    fn render_overview(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let render_markdown = |markdown: &Entity<Markdown>, window: &mut Window, cx: &App| {
            WithRemSize::new(ThemeSettings::get_global(cx).markdown_preview_font_size(cx))
                .w_full()
                .min_w_0()
                .child(
                    MarkdownElement::new(
                        markdown.clone(),
                        MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
                    )
                    .input_focus_handle(self.focus_handle.clone())
                    .scroll_handle(self.scroll_handle.clone())
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                        wrap_button_visibility: WrapButtonVisibility::VisibleOnHover,
                        border: true,
                    }),
                )
                .into_any_element()
        };

        v_flex()
            .debug_selector(|| "pr-overview".into())
            .w_full()
            .min_w_0()
            .gap_3()
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new(
                            "pr-overview-build",
                            if self.overview.is_some() {
                                "Rebuild overview"
                            } else {
                                "Build overview"
                            },
                        )
                        .tab_index(0_isize)
                        .disabled(self.overview_loading || self.detail.is_none())
                        .tooltip(Tooltip::text(
                            "Summarizes the changed files and symbols locally; downloads the PR objects into temporary storage",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.build_overview(cx))),
                    )
                    .child(
                        Button::new("pr-overview-narrate", "Explain with AI")
                            .tab_index(0_isize)
                            .disabled(
                                self.overview_summary.is_none()
                                    || self.overview_narrating
                                    || self.overview_loading,
                            )
                            .tooltip(Tooltip::text(
                                "Sends only the file and symbol structure, never the code, to the configured model",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.narrate_overview(cx))),
                    ),
            )
            .child(
                Label::new(
                    "Structure is computed locally from the diff. The optional explanation is generated from that structure alone.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.overview_loading, |this| {
                this.child(Label::new("Building overview\u{2026}").color(Color::Muted))
            })
            .when(self.overview_narrating, |this| {
                this.child(Label::new("Explaining changes\u{2026}").color(Color::Muted))
            })
            .when_some(self.overview_error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error))
            })
            .when(
                self.overview.is_none() && !self.overview_loading && self.overview_error.is_none(),
                |this| {
                    this.child(
                        Label::new("No overview yet. Build one to see which files and symbols this pull request touches.")
                            .color(Color::Muted),
                    )
                },
            )
            .when_some(self.overview.clone(), |this, markdown| {
                this.child(render_markdown(&markdown, window, cx))
            })
            .when_some(self.overview_narration.clone(), |this, markdown| {
                this.child(render_markdown(&markdown, window, cx))
            })
            .into_any_element()
    }

    fn render_markdown(&self) -> Option<AnyElement> {
        let markdown = self.markdown.clone()?;
        let menu_markdown = markdown.clone();
        let menu_focus = self.focus_handle.clone();
        let focus_handle = self.focus_handle.clone();
        let scroll_handle = self.scroll_handle.clone();
        Some(
            right_click_menu("pr-description-menu")
                .trigger(move |_, window, cx| {
                    div()
                        .debug_selector(|| "pr-markdown".into())
                        .w_full()
                        .child(
                            MarkdownElement::new(
                                markdown,
                                MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
                            )
                            .input_focus_handle(focus_handle)
                            .scroll_handle(scroll_handle)
                            .code_block_renderer(
                                CodeBlockRenderer::Default {
                                    copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                                    wrap_button_visibility: WrapButtonVisibility::VisibleOnHover,
                                    border: true,
                                },
                            ),
                        )
                })
                .maybe_menu(move |window, cx| {
                    let markdown = menu_markdown.read(cx);
                    let text = markdown.context_menu_selected_text().cloned();
                    let source = markdown.context_menu_selected_markdown().cloned();
                    let link = markdown.context_menu_link().cloned();
                    if text.is_none() && source.is_none() && link.is_none() {
                        return None;
                    }
                    let focus = menu_focus.clone();
                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.context(focus)
                            .when_some(text, |menu, text| {
                                menu.entry("Copy", Some(Box::new(markdown::Copy)), move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        text.to_string(),
                                    ))
                                })
                            })
                            .when_some(source, |menu, source| {
                                menu.entry(
                                    "Copy as Markdown",
                                    Some(Box::new(markdown::CopyAsMarkdown)),
                                    move |_, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            source.to_string(),
                                        ))
                                    },
                                )
                            })
                            .when_some(link, |menu, link| {
                                menu.entry("Copy link", None, move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        link.to_string(),
                                    ))
                                })
                            })
                    }))
                })
                .into_any_element(),
        )
    }
}

impl Render for PullRequestDescription {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let markdown = if self.section == DetailSection::Description {
            self.render_markdown()
        } else {
            None
        };
        v_flex()
            .id("pr-description")
            .size_full()
            .min_h_0()
            .min_w_0()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("pr-body")
                    .debug_selector(|| "pr-body".into())
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .p_4()
                            .gap_4()
                            .when(self.details_expanded, |this| {
                                this.child(self.render_details(cx))
                            })
                            .when(self.section == DetailSection::Activity, |this| {
                                this.children(self.activity.clone())
                            })
                            .when(self.section == DetailSection::Overview, |this| {
                                this.child(self.render_overview(window, cx))
                            })
                            .when_some(markdown, |this, markdown| {
                                // Preview typography belongs to the body, not the native controls.
                                this.child(
                                    WithRemSize::new(
                                        ThemeSettings::get_global(cx)
                                            .markdown_preview_font_size(cx),
                                    )
                                    .w_full()
                                    .min_w_0()
                                    .child(
                                        v_flex()
                                            .w_full()
                                            .min_w_0()
                                            .max_w(rems(DESCRIPTION_WIDTH_REMS))
                                            .mx_auto()
                                            .child(markdown),
                                    ),
                                )
                            }),
                    )
                    .vertical_scrollbar_for(&self.scroll_handle, window, cx),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Buttons activate on release; simulate_keystrokes only sends key-down.
    fn press_enter(cx: &mut gpui::VisualTestContext) {
        cx.refresh().expect("paint keyboard focus");
        cx.run_until_parked();
        let os = cx.update(|_, cx| cx.platform_info().os);
        let keystroke = gpui::Keystroke::parse_with_os("enter", os).expect("enter");
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
    }

    #[gpui::test]
    async fn test_pr_detail_layout(cx: &mut gpui::TestAppContext) {
        use gpui::UpdateGlobal;
        use settings::Settings;

        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            git::GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
            settings::SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(r#"{"markdown_preview":{"font_size":14}}"#, cx)
                    .expect("initial preview font");
            });
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        let project = Project::test(fs, [], cx).await;
        let target = cx.update(|cx| {
            GitHubRepository::from_remote_url(
                git::GitHostingProviderRegistry::default_global(cx),
                "https://github.com/example/project.git",
            )
            .expect("target")
        });
        let window = cx.add_window(|_, cx| {
            let mut description = PullRequestDescription::new(
                project, None, WeakEntity::new_invalid(), target, 1, cx,
            );
            description.set_detail(serde_json::from_value(serde_json::json!({
                "number": 1, "title": "A descriptive pull request title that wraps in a narrow pane",
                "author": {"login": "author"}, "isDraft": false, "state": "OPEN",
                "body": "A short paragraph.\n\n".repeat(80),
                "baseRefName": "main", "headRefName": "feature-with-a-very-long-branch-name".repeat(4),
                "baseRefOid": "1".repeat(40), "headRefOid": "2".repeat(40)
            })).expect("detail"), cx);
            description.error = None;
            description
        });
        let description = window.root(cx).expect("description");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        for width in [320., 1200.] {
            cx.simulate_resize(gpui::size(px(width), px(600.)));
            cx.refresh().expect("render detail");
            cx.run_until_parked();
            let header = cx.debug_bounds("pr-header").expect("native header");
            let actions = cx.debug_bounds("pr-actions").expect("native actions");
            assert!(
                cx.debug_bounds("pr-activity-toggle").is_some(),
                "activity navigation in native header"
            );
            let body = cx.debug_bounds("pr-body").expect("scrolling body");
            assert!(header.right() <= px(width));
            assert!(actions.right() <= px(width));
            assert!(body.top() >= actions.bottom());
            assert!(body.bottom() <= px(600.));
            for selector in [
                "pr-title",
                "pr-head-branch",
                "pr-base-branch",
                "ICON-RefreshTitle",
                "pr-native-control",
                "pr-github-control",
                "pr-details-toggle",
            ] {
                let child = cx.debug_bounds(selector).expect("header descendant");
                assert!(
                    child.left() >= header.left() && child.right() <= header.right(),
                    "overflow: {selector}"
                );
                assert!(
                    child.top() >= header.top() && child.bottom() <= header.bottom(),
                    "vertical overflow: {selector}"
                );
            }
            assert!(cx.debug_bounds("pr-revision-details").is_none());
            let markdown = cx.debug_bounds("pr-markdown").expect("description");
            cx.simulate_event(gpui::ScrollEvent {
                position: body.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-200.))),
                modifiers: Default::default(),
                touch_phase: gpui::TouchPhase::Moved,
                source: gpui::ScrollSource::Pointer,
            });
            cx.refresh().expect("scroll detail");
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds("pr-header"), Some(header));
            assert_eq!(cx.debug_bounds("pr-actions"), Some(actions));
            assert!(
                cx.debug_bounds("pr-markdown")
                    .expect("scrolled description")
                    .top()
                    < markdown.top()
            );
            cx.update(|window, cx| {
                window.blur(cx);
                // Advance through Refresh, View changes and GitHub to Details.
                for _ in ["Refresh", "View changes", "GitHub", "Details"] {
                    window.focus_next(cx);
                }
            });
            press_enter(&mut cx);
            cx.refresh().expect("expand details by keyboard");
            cx.run_until_parked();
            let expanded = cx
                .debug_bounds("pr-revision-details")
                .expect("keyboard-expanded details");
            assert!(expanded.top() >= body.top());
            assert!(expanded.bottom() <= body.bottom());
            assert!(expanded.left() >= body.left() && expanded.right() <= body.right());

            // The section buttons follow Details before the expanded SHA controls.
            for _ in ["Description", "Overview", "Activity"] {
                cx.update(|window, cx| window.focus_next(cx));
            }
            for expected in ["1".repeat(40), "2".repeat(40)] {
                cx.update(|window, cx| window.focus_next(cx));
                press_enter(&mut cx);
                let copied =
                    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
                assert_eq!(copied, Some(expected), "copy full commit ID by keyboard");
            }
            let details = cx
                .debug_bounds("pr-details-toggle")
                .expect("details toggle");
            cx.simulate_click(details.center(), gpui::Modifiers::default());
            cx.refresh().expect("collapse details");
            cx.run_until_parked();
            assert!(cx.debug_bounds("pr-revision-details").is_none());
        }
        description.update(&mut cx, |description, cx| {
            let detail = description.detail.as_mut().expect("detail");
            detail.head_ref_name = detail.base_ref_name.clone();
            cx.notify();
        });
        cx.refresh().expect("equal fork branch names");
        cx.run_until_parked();

        let before = cx
            .debug_bounds("pr-markdown")
            .expect("preview font baseline")
            .size;
        let actions = cx.debug_bounds("pr-actions");
        cx.update(|_, cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(r#"{"markdown_preview":{"font_size":28}}"#, cx)
                    .expect("preview settings");
            });
            assert_eq!(
                theme_settings::ThemeSettings::get_global(cx).markdown_preview_font_size(cx),
                px(28.)
            );
        });
        cx.refresh().expect("preview font sizing");
        cx.run_until_parked();
        let after = cx.debug_bounds("pr-markdown").expect("larger preview").size;
        assert!(after.height > before.height);
        assert!(
            after.width > before.width,
            "reading width must use preview rem sizing"
        );
        assert_eq!(
            cx.debug_bounds("pr-actions"),
            actions,
            "preview font must not resize controls"
        );
    }

    #[gpui::test]
    async fn test_remote_save_retry(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let store = cx.update(|cx| KeyValueStore::global(cx));
        store.write(|connection| {
            connection.exec("CREATE TRIGGER fail_remote_save BEFORE INSERT ON scoped_kv_store BEGIN SELECT RAISE(FAIL, 'simulated write failure'); END")?()
        }).await.expect("install write failure");
        let key = "retry-repository";
        let selection = RemoteSelection {
            name: "origin".into(),
            target: "github.com/example/project".into(),
        };
        cx.update(|cx| RemoteSelections::save(key.into(), selection.clone(), cx));
        let save = cx
            .update(|cx| cx.default_global::<RemoteSelections>().save_task.take())
            .expect("pending save");
        save.await;
        assert!(
            store
                .scoped(REMOTE_SELECTION_KEY)
                .read(key)
                .expect("read failed preference")
                .is_none()
        );
        store
            .write(|connection| connection.exec("DROP TRIGGER fail_remote_save")?())
            .await
            .expect("repair database");
        cx.update(|cx| RemoteSelections::save(key.into(), selection.clone(), cx));
        let save = cx.update(|cx| cx.default_global::<RemoteSelections>().save_task.take());
        if let Some(save) = save {
            save.await;
        }
        let value = store
            .scoped(REMOTE_SELECTION_KEY)
            .read(key)
            .expect("read retried preference")
            .expect("same-choice retry must persist after repair");
        assert!(
            serde_json::from_str::<RemoteSelection>(&value).expect("saved selection") == selection
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_remote_path_keys() {
        use std::os::unix::ffi::OsStringExt as _;
        let first = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\x80".to_vec()));
        let second = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\x81".to_vec()));
        assert_ne!(remote_key(&first), remote_key(&second));
    }

    #[gpui::test]
    async fn test_remote_defaults(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            git::GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        let project = Project::test(fs, [], cx).await;
        let browser =
            cx.new(|cx| PullRequestBrowser::new(project, None, WeakEntity::new_invalid(), cx));
        browser.update(cx, |browser, cx| {
            let generation = browser.state.generation;
            browser.set_remotes(
                collections::HashMap::from_iter([(
                    "origin".into(),
                    "https://github.com/example/fork.git".into(),
                )]),
                cx,
            );
            assert_eq!(
                browser.selected_remote,
                Some(0),
                "select the only GitHub remote"
            );
            assert_eq!(
                browser.state.generation,
                generation + 1,
                "automatic selection must refresh"
            );
            browser.set_remotes(
                collections::HashMap::from_iter([
                    (
                        "upstream".into(),
                        "https://github.com/example/project.git".into(),
                    ),
                    (
                        "origin".into(),
                        "https://github.com/example/fork.git".into(),
                    ),
                    (
                        "other".into(),
                        "https://gitlab.com/example/project.git".into(),
                    ),
                ]),
                cx,
            );
            assert_eq!(browser.remotes.len(), 2);
            assert_eq!(browser.selected_remote, Some(0));
            assert_eq!(
                browser.remotes[0].0, "origin",
                "default to the first sorted GitHub remote"
            );
        });
    }

    #[gpui::test(iterations = 5)]
    async fn test_remote_memory(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            git::GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            util::path!("/"),
            serde_json::json!({
                "first": { ".git": {} }, "second": { ".git": {} }
            }),
        )
        .await;
        let project = Project::test(
            fs,
            [
                std::path::Path::new(util::path!("/first")),
                std::path::Path::new(util::path!("/second")),
            ],
            cx,
        )
        .await;
        cx.run_until_parked();
        let mut repositories = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>()
        });
        repositories.sort_by_key(|repository| {
            repository.read_with(cx, |repository, _| {
                repository.work_directory_abs_path.clone()
            })
        });
        let first = repositories.first().expect("first repository").clone();
        let second = repositories.get(1).expect("second repository").clone();
        cx.update(|cx| {
            assert!(
                PullRequestBrowser::preload(
                    project.clone(),
                    first.clone(),
                    WeakEntity::new_invalid(),
                    cx
                )
                .is_none(),
                "never prefetch untrusted repositories"
            );
            assert!(cx.try_global::<PullRequestCache>().is_none());
        });
        let remotes = collections::HashMap::from_iter([
            (
                "upstream".into(),
                "https://github.com/example/project.git".into(),
            ),
            (
                "origin".into(),
                "https://github.com/example/fork.git".into(),
            ),
        ]);
        let browser = cx.new(|cx| {
            PullRequestBrowser::new(
                project.clone(),
                Some(first.clone()),
                WeakEntity::new_invalid(),
                cx,
            )
        });
        browser.update(cx, |browser, cx| {
            browser.set_remotes(remotes.clone(), cx);
            assert_eq!(browser.selected_remote, Some(0));
            let generation = browser.state.generation;
            browser.select_remote(1, cx);
            assert_eq!(
                browser.state.generation,
                generation + 1,
                "explicit selection must refresh"
            );
            browser.select_remote(0, cx);
            browser.select_remote(1, cx);
            browser.set_remotes(remotes.clone(), cx);
            assert_eq!(
                browser.selected_remote,
                Some(1),
                "reload must retain an unsaved selection"
            );
            assert!(
                browser.state.generation > generation,
                "selection must refresh the list"
            );
            assert!(
                browser
                    .state
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("trust")),
                "automatic refresh must retain the trust gate without invoking gh"
            );
        });
        // Recreate before yielding: none of the queued saves can have completed.
        drop(browser);

        let restored = cx.new(|cx| {
            PullRequestBrowser::new(
                project.clone(),
                Some(first.clone()),
                WeakEntity::new_invalid(),
                cx,
            )
        });
        restored.update(cx, |browser, cx| {
            browser.set_remotes(remotes.clone(), cx);
            assert_eq!(
                browser.selected_remote,
                Some(1),
                "pending choice must survive browser recreation"
            );
        });
        let other = cx.new(|cx| {
            PullRequestBrowser::new(project.clone(), Some(second), WeakEntity::new_invalid(), cx)
        });
        other.update(cx, |browser, cx| {
            browser.set_remotes(remotes.clone(), cx);
            assert_eq!(
                browser.selected_remote,
                Some(0),
                "selection must not leak between repositories"
            );
        });
        let save = cx.update(|cx| cx.default_global::<RemoteSelections>().save_task.take());
        if let Some(save) = save {
            save.await;
        }
        drop(restored);
        drop(other);
        cx.update(|cx| cx.set_global(RemoteSelections::default()));

        let restored = cx
            .new(|cx| PullRequestBrowser::new(project, Some(first), WeakEntity::new_invalid(), cx));
        restored.update(cx, |browser, cx| {
            browser.set_remotes(remotes.clone(), cx);
            assert_eq!(
                browser.selected_remote,
                Some(1),
                "restore persisted selection without the memory cache"
            );
            let mut changed = remotes.clone();
            changed.insert(
                "upstream".into(),
                "https://github.com/example/replacement.git".into(),
            );
            browser.set_remotes(changed, cx);
            assert_eq!(
                browser.selected_remote,
                Some(0),
                "retargeted remote must fall back"
            );
            browser.select_remote(1, cx);
            browser.set_remotes(
                collections::HashMap::from_iter([(
                    "origin".into(),
                    "https://github.com/example/fork.git".into(),
                )]),
                cx,
            );
            assert_eq!(
                browser.selected_remote,
                Some(0),
                "removed remote must fall back"
            );
            browser.set_remotes(collections::HashMap::default(), cx);
            assert_eq!(browser.selected_remote, None);
            assert!(
                browser
                    .state
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("No recognized GitHub remotes"))
            );
        });
        let save = cx.update(|cx| cx.default_global::<RemoteSelections>().save_task.take());
        if let Some(save) = save {
            save.await;
        }
        let key = restored.read_with(cx, |browser, _| {
            browser.selection_key.clone().expect("repository key")
        });
        let store = cx.update(|cx| KeyValueStore::global(cx));
        store
            .scoped(REMOTE_SELECTION_KEY)
            .write(key, "invalid JSON".into())
            .await
            .expect("write corrupt preference");
        cx.update(|cx| cx.set_global(RemoteSelections::default()));
        restored.update(cx, |browser, cx| {
            browser.set_remotes(remotes, cx);
            assert_eq!(
                browser.selected_remote,
                Some(0),
                "corrupt preferences must fall back"
            );
        });
        let save = cx.update(|cx| cx.default_global::<RemoteSelections>().save_task.take());
        if let Some(save) = save {
            save.await;
        }
    }

    #[gpui::test]
    async fn test_pr_cached_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        let project = Project::test(fs, [], cx).await;
        let browser =
            cx.new(|cx| PullRequestBrowser::new(project, None, WeakEntity::new_invalid(), cx));
        let entries: Vec<PullRequestSummary> = serde_json::from_value(serde_json::json!([
            {"number": 1, "title": "First", "author": null, "isDraft": false},
            {"number": 2, "title": "Second", "author": null, "isDraft": false},
            {"number": 3, "title": "Third", "author": null, "isDraft": false},
            {"number": 4, "title": "Fourth", "author": null, "isDraft": false},
            {"number": 5, "title": "Selected", "author": null, "isDraft": false}
        ]))
        .expect("cached list fixture");
        let generation = browser.update(cx, |browser, _| {
            let generation = browser.state.begin();
            browser.state.entries = entries.clone();
            generation
        });
        let (sender, receiver) = futures::channel::oneshot::channel();
        let response = cx
            .background_executor
            .spawn(async move { receiver.await.expect("delayed list") });
        browser.update(cx, |browser, _| browser.selected_entry = Some(4));
        sender
            .send(vec![entries[4].clone(), entries[0].clone()])
            .expect("finish revalidation");
        let entries = response.await;
        browser.update(cx, |browser, _| {
            browser.apply_list(generation, Ok(entries.clone()));
            assert_eq!(
                browser.selected_entry,
                Some(0),
                "retain selection by PR number, not row index"
            );
            browser.apply_list(generation - 1, Ok(Vec::new()));
            assert_eq!(
                browser.selected_entry,
                Some(0),
                "ignore an obsolete response"
            );
            browser.apply_list(generation, Ok(vec![entries[1].clone()]));
            assert_eq!(browser.selected_entry, None, "clear a removed selection");
        });
    }

    #[gpui::test]
    async fn test_pr_markdown(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            git::GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        let project = Project::test(fs, [], cx).await;
        project.read_with(cx, |project, cx| {
            project
                .languages()
                .register_test_language(language::LanguageConfig {
                    name: "Demo".into(),
                    ..Default::default()
                });
            assert!(cx.try_global::<PullRequestCache>().is_none());
        });
        let target = cx.update(|cx| {
            GitHubRepository::from_remote_url(
                git::GitHostingProviderRegistry::default_global(cx),
                "https://github.com/example/project.git",
            )
            .expect("GitHub target")
        });
        let description = cx.new(|cx| {
            PullRequestDescription::new(project, None, WeakEntity::new_invalid(), target, 1, cx)
        });
        let body = "# Preview\n\n**Description** with [a link](https://example.com).\n\n```demo\nlet value = 1;\n```\n\n| File | Change |\n| --- | --- |\n| a | added |\n";
        description.update(cx, |description, cx| {
            let detail = serde_json::from_value(serde_json::json!({
                "number": 1, "title": "Preview fixture", "author": {"login": "author"}, "isDraft": false,
                "body": body, "state": "OPEN", "baseRefName": "main", "headRefName": "feature",
                "baseRefOid": "1".repeat(40), "headRefOid": "2".repeat(40)
            })).expect("description fixture");
            description.set_detail(detail, cx);
        });
        cx.run_until_parked();
        description.read_with(cx, |description, cx| {
            let markdown = description
                .markdown
                .as_ref()
                .expect("Markdown preview")
                .read(cx);
            assert_eq!(markdown.source().as_ref(), body);
            assert_eq!(
                markdown
                    .first_code_block_language()
                    .expect("use project languages for code blocks")
                    .name()
                    .as_ref(),
                "Demo"
            );
        });
    }

    #[gpui::test(iterations = 5)]
    async fn test_pr_preview_completion(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            git::GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(util::path!("/repo"), serde_json::json!({".git": {}}))
            .await;
        let project = Project::test(fs, [std::path::Path::new(util::path!("/repo"))], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .repositories(cx)
                .values()
                .next()
                .expect("repository")
                .clone()
        });
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |root, _| root.workspace().clone())
            .expect("workspace");
        let target = cx.update(|cx| {
            GitHubRepository::from_remote_url(
                git::GitHostingProviderRegistry::default_global(cx),
                "https://github.com/example/project.git",
            )
            .expect("target")
        });
        let description = cx.new(|cx| {
            PullRequestDescription::new(
                project.clone(),
                Some(repository.clone()),
                workspace.downgrade(),
                target,
                1,
                cx,
            )
        });
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(description.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
                project::trusted_worktrees::init(Default::default(), cx);
                let snapshot = || RevisionDiff {
                    base: Oid::from_str(&"1".repeat(40)).expect("base"),
                    head: Oid::from_str(&"2".repeat(40)).expect("head"),
                    files: Vec::new(),
                };
                let complete = |result, window: &mut Window, cx: &mut App| {
                    description.update(cx, |description, _| description.diff_loading = true);
                    PullRequestDescription::finish_diff(
                        &description.downgrade(),
                        result,
                        PreviewRequest {
                            generation: description.read(cx).preview_generation,
                            mode: RevisionDiffMode::MergeBase,
                            repository: repository.clone(),
                            title: "PR changes".into(),
                            reveal_file: None,
                        },
                        window,
                        cx,
                    );
                };
                complete(Ok(snapshot()), window, cx);
                assert!(!description.read(cx).diff_loading);
                assert!(description.read(cx).diff_error.is_some());
                assert!(
                    workspace
                        .read(cx)
                        .active_item_as::<PullRequestDescription>(cx)
                        .is_some()
                );

                let store = project.read(cx).worktree_store();
                let paths = project
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|tree| project::trusted_worktrees::PathTrust::Worktree(tree.read(cx).id()))
                    .collect();
                TrustedWorktrees::try_get_global(cx)
                    .expect("trust store")
                    .update(cx, |trusted, cx| {
                        trusted.trust(&store, paths, cx);
                    });
                check_access(&project, cx).expect("trusted fixture");
                const FETCH_ERROR: &str = "fixture fetch failure";
                complete(Err(anyhow::anyhow!(FETCH_ERROR)), window, cx);
                assert!(!description.read(cx).diff_loading);
                assert_eq!(
                    description.read(cx).diff_error.as_deref(),
                    Some(FETCH_ERROR)
                );
                assert!(
                    workspace
                        .read(cx)
                        .active_item_as::<PullRequestDescription>(cx)
                        .is_some()
                );

                complete(Ok(snapshot()), window, cx);
                assert!(
                    workspace
                        .read(cx)
                        .active_item_as::<RevisionDiffView>(cx)
                        .is_some()
                );
                assert_eq!(
                    workspace
                        .read(cx)
                        .items_of_type::<PullRequestDescription>(cx)
                        .count(),
                    1
                );
                assert!(!description.read(cx).diff_loading);
                assert!(description.read(cx).diff_error.is_none());
            })
            .expect("preview completion");
    }

    #[gpui::test]
    async fn test_pr_row_click_target(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        let project = Project::test(fs, [], cx).await;
        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |root, _| root.workspace().clone())
            .expect("workspace");
        let window = cx.add_window(|_, cx| {
            let mut browser = PullRequestBrowser::new(project, None, workspace.downgrade(), cx);
            browser.state.error = None;
            browser.state.entries = serde_json::from_value(serde_json::json!([
                {"number": 1, "title": "A long pull request title that must truncate rather than clip its beginning".repeat(5), "author": {"login": "author"}, "isDraft": false},
                {"number": 2, "title": "Short title", "author": {"login": "author"}, "isDraft": true}
            ])).expect("PR fixture");
            browser.refresh_rows();
            browser
        });
        let browser = window.root(cx).expect("browser");
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(gpui::size(px(320.), px(600.)));
        cx.refresh().expect("render rows");
        cx.run_until_parked();
        let row = cx.debug_bounds("pr-row-0").expect("rendered row");
        let next_row = cx.debug_bounds("pr-row-1").expect("second row");
        assert_eq!(row.size, next_row.size, "long titles must not grow rows");
        assert_eq!(row.left(), next_row.left());
        assert!(row.right() <= px(320.));
        assert_eq!(row.bottom(), next_row.top());

        // The metadata line must open the row too, just like History.
        cx.simulate_click(
            gpui::point(row.left() + px(20.), row.bottom() - px(10.)),
            gpui::Modifiers::default(),
        );
        browser.read_with(&cx, |browser, _| {
            assert_eq!(browser.selected_entry, Some(0))
        });
    }

    #[test]
    fn test_pull_request_browser_rows_nest_commits_and_files() {
        use crate::commit_file_list::CommitFileSummary;
        use git::repository::{CommitFileStatus, RepoPath};
        use git::status::DiffStat;

        let mut state = ListState::default();
        state.entries = serde_json::from_value(serde_json::json!([
            {"number": 1, "title": "First", "author": {"login": "author"}, "isDraft": false},
            {"number": 2, "title": "Second", "author": {"login": "author"}, "isDraft": false}
        ]))
        .expect("PR fixture");

        let commit = Oid::from_bytes(&[7; 20]).expect("oid");
        let mut pull_request_commits = HashMap::default();
        pull_request_commits.insert(1, PullRequestCommits::Loading);
        assert_eq!(
            browser_rows(&state.entries, &pull_request_commits, &HashMap::default()).to_vec(),
            vec![
                BrowserRow::PullRequest { index: 0 },
                BrowserRow::PullRequestMessage { index: 0 },
                BrowserRow::PullRequest { index: 1 },
            ],
            "a pull request that is still loading its commits must show feedback"
        );

        pull_request_commits.insert(
            1,
            PullRequestCommits::Loaded(
                vec![PullRequestCommit {
                    oid: commit,
                    title: "Fix".into(),
                    author: "ada".into(),
                }]
                .into(),
            ),
        );
        let mut commit_files = HashMap::default();
        commit_files.insert(
            commit,
            CommitFileList::Loaded(
                vec![
                    CommitFileSummary {
                        path: RepoPath::from_rel_path(util::rel_path::rel_path("src/main.rs")),
                        status: CommitFileStatus::Modified,
                        stat: DiffStat {
                            added: 1,
                            deleted: 1,
                        },
                        is_binary: false,
                    },
                    CommitFileSummary {
                        path: RepoPath::from_rel_path(util::rel_path::rel_path("README.md")),
                        status: CommitFileStatus::Added,
                        stat: DiffStat {
                            added: 2,
                            deleted: 0,
                        },
                        is_binary: false,
                    },
                ]
                .into(),
            ),
        );
        assert_eq!(
            browser_rows(&state.entries, &pull_request_commits, &commit_files).to_vec(),
            vec![
                BrowserRow::PullRequest { index: 0 },
                BrowserRow::Commit {
                    index: 0,
                    commit_index: 0
                },
                BrowserRow::File {
                    index: 0,
                    commit_index: 0,
                    file_index: 0
                },
                BrowserRow::File {
                    index: 0,
                    commit_index: 0,
                    file_index: 1
                },
                BrowserRow::PullRequest { index: 1 },
            ],
            "expanded commits must list their changed files under the commit"
        );
    }

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
