use super::{DESCRIPTION_WIDTH_REMS, check_access};
use crate::commit_file_list::{CommitFileList, CommitFileSummary, render_commit_file};
use crate::github_pull_requests::{
    ACTIVITY_LIMIT, ACTIVITY_PAGE_SIZE, ActivityItem, ActivityKind, ActivityPage, ActivitySection,
    GitHubPullRequestService, GitHubRepository,
};
use anyhow::{Result, ensure};
use collections::HashMap;
use git::Oid;
use git::repository::RepoPath;
use gpui::{App, Context, Entity, FocusHandle, ScrollHandle, Task, Window};
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownStyle, WrapButtonVisibility,
};
use project::Project;
use settings::Settings;
use std::{rc::Rc, sync::Arc, time::Duration};
use theme_settings::ThemeSettings;
use ui::{prelude::*, utils::WithRemSize};
use util::ResultExt as _;

const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVITY_TEXT_LIMIT: usize = 4 * 1024 * 1024;
const SHORT_COMMIT_LENGTH: usize = 8;
const MIN_CODE_FENCE: usize = 3;

/// Opens a commit's diff, optionally revealing one repository-relative path first.
type CommitCallback = Rc<dyn Fn(Oid, Option<RepoPath>, &mut Window, &mut App)>;
/// Loads the files a PR commit touches, summarized for list rendering.
type CommitFilesCallback = Rc<dyn Fn(Oid, &mut App) -> Task<Result<Vec<CommitFileSummary>>>>;
type ActivityLoader = Arc<
    dyn Fn(
            GitHubRepository,
            u64,
            ActivitySection,
            Option<String>,
        ) -> futures::future::BoxFuture<'static, Result<ActivityPage>>
        + Send
        + Sync,
>;

#[derive(Clone, Copy)]
pub(super) enum PreviewState {
    Ready,
    Loading,
}

#[derive(Clone, Copy)]
enum MarkdownPart {
    Body,
    Context,
}

#[derive(Clone, Copy)]
enum LoadMode {
    Replace,
    Append,
}

#[derive(Default)]
struct ActivityState {
    generation: usize,
    items: Vec<ActivityItem>,
    head: Option<String>,
    cursor: Option<String>,
    pages: usize,
    loading: bool,
    error: Option<String>,
}

impl ActivityState {
    fn start(&mut self, mode: LoadMode) -> usize {
        if matches!(mode, LoadMode::Replace) {
            self.items.clear();
            self.head = None;
            self.cursor = None;
            self.pages = 0;
        }
        self.generation += 1;
        self.loading = true;
        self.error = None;
        self.generation
    }

    fn complete(&mut self, generation: usize, page: Result<ActivityPage>) -> Result<()> {
        if generation != self.generation {
            return Ok(());
        }
        self.loading = false;
        let page = page?;
        ensure!(
            self.head.as_ref().is_none_or(|head| *head == page.head),
            "PR head changed while paging. Refresh activity before loading more"
        );
        ensure!(
            page.cursor.is_none() || page.cursor != self.cursor,
            "GitHub repeated an activity cursor; refresh or open GitHub"
        );
        let new_items: Vec<_> = page
            .items
            .into_iter()
            .filter(|item| !self.items.iter().any(|existing| existing.id == item.id))
            .collect();
        ensure!(
            self.items.len() + new_items.len() <= ACTIVITY_LIMIT,
            "Activity entry limit reached; open GitHub for the rest"
        );
        let text_size = self
            .items
            .iter()
            .chain(&new_items)
            .map(item_text_size)
            .sum::<usize>();
        ensure!(
            text_size <= ACTIVITY_TEXT_LIMIT,
            "Activity text limit reached; open GitHub for the rest"
        );
        self.items.extend(new_items);
        self.head = Some(page.head);
        self.cursor = page.cursor;
        self.pages += 1;
        Ok(())
    }

    fn can_load_more(&self) -> bool {
        self.cursor.is_some() && self.pages < ACTIVITY_LIMIT / ACTIVITY_PAGE_SIZE
    }
}

fn item_text_size(item: &ActivityItem) -> usize {
    item.body.len()
        + match &item.kind {
            ActivityKind::Thread { comments, .. } => comments
                .iter()
                .map(|comment| comment.body.len() + comment.hunk.len())
                .sum(),
            ActivityKind::Commit { title, .. } => title.len(),
            _ => 0,
        }
}

struct CommentView {
    body: Entity<Markdown>,
    context: Option<Entity<Markdown>>,
}

struct ItemView {
    body: Option<Entity<Markdown>>,
    comments: Vec<CommentView>,
}

pub(super) struct ActivityReloaded;

pub(super) struct PullRequestActivity {
    project: Entity<Project>,
    target: GitHubRepository,
    number: u64,
    section: ActivitySection,
    state: ActivityState,
    markdown: HashMap<String, ItemView>,
    task: Task<()>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    on_commit: CommitCallback,
    load_commit_files: CommitFilesCallback,
    commit_files: HashMap<Oid, CommitFileList>,
    commit_file_tasks: HashMap<Oid, Task<()>>,
    load_page: ActivityLoader,
    busy: bool,
}

impl gpui::EventEmitter<ActivityReloaded> for PullRequestActivity {}

impl PullRequestActivity {
    pub(super) fn new(
        project: Entity<Project>,
        target: GitHubRepository,
        number: u64,
        scroll: ScrollHandle,
        on_commit: CommitCallback,
        load_commit_files: CommitFilesCallback,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            project,
            target,
            number,
            section: ActivitySection::Timeline,
            state: ActivityState::default(),
            markdown: HashMap::default(),
            task: Task::ready(()),
            focus: cx.focus_handle(),
            scroll,
            on_commit,
            load_commit_files,
            commit_files: HashMap::default(),
            commit_file_tasks: HashMap::default(),
            load_page: Arc::new(|target, number, section, cursor| {
                Box::pin(async move {
                    GitHubPullRequestService::activity(&target, number, section, cursor).await
                })
            }),
            busy: false,
        }
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(LoadMode::Replace, cx);
    }

    fn toggle_commit_files(&mut self, oid: Oid, cx: &mut Context<Self>) {
        if self.commit_files.remove(&oid).is_some() {
            self.commit_file_tasks.remove(&oid);
            cx.notify();
            return;
        }
        let files = (self.load_commit_files)(oid, cx);
        self.commit_files.insert(oid, CommitFileList::Loading);
        self.commit_file_tasks.insert(
            oid,
            cx.spawn(async move |this, cx| {
                let files = files.await;
                this.update(cx, |this, cx| {
                    // The row may have been collapsed, or the activity reloaded, while loading.
                    if !this.commit_files.contains_key(&oid) {
                        return;
                    }
                    let state = match files {
                        Ok(files) => CommitFileList::Loaded(files.into()),
                        Err(error) => CommitFileList::Error(format!("{error:#}").into()),
                    };
                    this.commit_files.insert(oid, state);
                    cx.notify();
                })
                .log_err();
            }),
        );
        cx.notify();
    }

    pub(super) fn set_preview(&mut self, state: PreviewState, cx: &mut Context<Self>) {
        self.busy = matches!(state, PreviewState::Loading);
        cx.notify();
    }

    fn load(&mut self, mode: LoadMode, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        let generation = self.state.start(mode);
        if matches!(mode, LoadMode::Replace) {
            self.markdown.clear();
            self.commit_files.clear();
            self.commit_file_tasks.clear();
            self.busy = false;
            cx.emit(ActivityReloaded);
        }
        if let Err(error) = check_access(&self.project, cx) {
            self.state.loading = false;
            self.state.error = Some(format!("{error:#}"));
            self.state.items.clear();
            self.markdown.clear();
            cx.notify();
            return;
        }
        let target = self.target.clone();
        let number = self.number;
        let section = self.section;
        let cursor = self.state.cursor.clone();
        let executor = cx.background_executor().clone();
        let load_page = self.load_page.clone();
        let response = cx.background_spawn(async move {
            use futures::{FutureExt as _, select_biased};
            let request = load_page(target, number, section, cursor).fuse();
            let timeout = executor.timer(ACTIVITY_TIMEOUT).fuse();
            futures::pin_mut!(request, timeout);
            select_biased! {
                result = request => result,
                () = timeout => anyhow::bail!("Loading activity timed out; refresh or open GitHub"),
            }
        });
        self.task = cx.spawn(async move |this, cx| {
            let result = response.await;
            this.update(cx, |this, cx| {
                if generation != this.state.generation {
                    return;
                }
                let result = check_access(&this.project, cx).and(result);
                if let Err(error) = this.state.complete(generation, result) {
                    this.state.error = Some(format!("{error:#}"));
                }
                if check_access(&this.project, cx).is_err() {
                    this.state.items.clear();
                    this.markdown.clear();
                }
                this.prepare_markdown(cx);
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn prepare_markdown(&mut self, cx: &mut Context<Self>) {
        let languages = self.project.read(cx).languages().clone();
        for item in &self.state.items {
            if self.markdown.contains_key(&item.id) {
                continue;
            }
            let body = if item.body.is_empty() {
                None
            } else {
                Some(cx.new(|cx| {
                    Markdown::new(item.body.clone().into(), Some(languages.clone()), None, cx)
                }))
            };
            let comments = match &item.kind {
                ActivityKind::Thread { comments, .. } => comments
                    .iter()
                    .map(|comment| {
                        let body = cx.new(|cx| {
                            Markdown::new(
                                comment.body.clone().into(),
                                Some(languages.clone()),
                                None,
                                cx,
                            )
                        });
                        let context = (!comment.hunk.is_empty()).then(|| {
                            let fence_length = comment
                                .hunk
                                .split(|character| character != '`')
                                .map(str::len)
                                .max()
                                .unwrap_or_default()
                                .saturating_add(1)
                                .max(MIN_CODE_FENCE);
                            let fence = "`".repeat(fence_length);
                            let source = format!("{fence}diff\n{}\n{fence}", comment.hunk);
                            cx.new(|cx| {
                                Markdown::new(source.into(), Some(languages.clone()), None, cx)
                            })
                        });
                        CommentView { body, context }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            self.markdown
                .insert(item.id.clone(), ItemView { body, comments });
        }
    }

    fn render_markdown(
        &self,
        markdown: &Entity<Markdown>,
        part: MarkdownPart,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        WithRemSize::new(ThemeSettings::get_global(cx).markdown_preview_font_size(cx))
            .w_full()
            .min_w_0()
            .child(
                v_flex()
                    // Code-block scrollbar IDs reuse source offsets across Markdown instances.
                    .id(("pr-activity-markdown", markdown.entity_id()))
                    .debug_selector(move || match part {
                        MarkdownPart::Body => "pr-review-body".into(),
                        MarkdownPart::Context => "pr-review-context".into(),
                    })
                    .w_full()
                    .min_w_0()
                    .max_w(rems(DESCRIPTION_WIDTH_REMS))
                    .child(
                        MarkdownElement::new(
                            markdown.clone(),
                            MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
                        )
                        .input_focus_handle(self.focus.clone())
                        .scroll_handle(self.scroll.clone())
                        .code_block_renderer(CodeBlockRenderer::Default {
                            copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                            wrap_button_visibility: WrapButtonVisibility::VisibleOnHover,
                            border: true,
                        }),
                    ),
            )
            .into_any_element()
    }

    fn render_item(
        &self,
        item: &ActivityItem,
        view: &ItemView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut row = v_flex()
            .id(item.id.clone())
            .w_full()
            .min_w_0()
            .p_3()
            .gap_2()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md();
        if !item.author.is_empty() {
            row = row.child(
                Label::new(format!("{}  {}", item.author, item.date))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        row = match &item.kind {
            ActivityKind::Commit { oid, title } => {
                let oid = *oid;
                let short: String = oid.to_string().chars().take(SHORT_COMMIT_LENGTH).collect();
                let on_commit = self.on_commit.clone();
                let is_expanded = self.commit_files.contains_key(&oid);
                let mut commit = row
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                IconButton::new(
                                    ElementId::Name(format!("pr-commit-files-{oid}").into()),
                                    if is_expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    },
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(ui::Tooltip::text(if is_expanded {
                                    "Hide Changed Files"
                                } else {
                                    "Show Changed Files"
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_commit_files(oid, cx)
                                })),
                            )
                            .child(
                                Label::new(format!("{short}  {title}"))
                                    .truncate()
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(div().debug_selector(|| "pr-commit-changes".into()).child(
                        Button::new(ElementId::Name(format!("pr-commit-changes-{oid}").into()), "View changes")
                            .tab_index(0_isize).aria_label(format!("View changes for commit {oid}"))
                            .disabled(self.busy)
                            .tooltip(ui::Tooltip::text("Read-only first-parent commit diff; missing objects are downloaded into temporary storage"))
                            .on_click(move |_, window, cx| on_commit(oid, None, window, cx))));
                match self.commit_files.get(&oid) {
                    None => {}
                    Some(CommitFileList::Loading) => {
                        commit = commit.child(
                            Label::new("Loading changed files…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        );
                    }
                    Some(CommitFileList::Error(error)) => {
                        commit = commit.child(
                            Label::new(error.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        );
                    }
                    Some(CommitFileList::Loaded(files)) if files.is_empty() => {
                        commit = commit.child(
                            Label::new("No files changed")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        );
                    }
                    Some(CommitFileList::Loaded(files)) => {
                        let container = v_flex()
                            .debug_selector(|| "pr-commit-files".into())
                            .w_full()
                            .min_w_0();
                        commit = commit.child(files.iter().enumerate().fold(
                            container,
                            |container, (index, file)| {
                                let on_commit = self.on_commit.clone();
                                let path = file.path.clone();
                                container.child(render_commit_file(
                                    ElementId::Name(format!("pr-commit-file-{oid}-{index}").into()),
                                    file,
                                    move |_, window, cx| {
                                        on_commit(oid, Some(path.clone()), window, cx)
                                    },
                                    cx,
                                ))
                            },
                        ));
                    }
                }
                commit
            }
            ActivityKind::Comment => row.child(
                Label::new("Comment")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            ActivityKind::Review { state } => row.child(
                Label::new(format!(
                    "Review · {}",
                    state.replace('_', " ").to_lowercase()
                ))
                .size(LabelSize::Small),
            ),
            ActivityKind::Event { name } => row.child(
                Label::new(format!("GitHub activity · {name}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            ActivityKind::Thread {
                path,
                line,
                resolved,
                outdated,
                comments,
                more_comments,
            } => {
                let location = line.map_or_else(|| path.clone(), |line| format!("{path}:{line}"));
                let status = format!(
                    "{}{}",
                    if *resolved {
                        " · Resolved"
                    } else {
                        " · Unresolved"
                    },
                    if *outdated { " · Outdated" } else { "" }
                );
                let mut thread = row
                    .child(
                        div()
                            .id("pr-thread-location")
                            .tooltip(ui::Tooltip::text(location.clone()))
                            .child(
                                Label::new(location)
                                    .truncate()
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(
                        Label::new(status.trim_start_matches(" · ").to_owned())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                for (index, (comment, markdown)) in comments.iter().zip(&view.comments).enumerate()
                {
                    let mut entry = v_flex()
                        .id(("pr-thread-comment", index))
                        .debug_selector(move || format!("pr-review-comment-{index}"))
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            Label::new(format!("{}  {}", comment.author, comment.date))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        );
                    if let Some(context) = &markdown.context {
                        entry = entry.child(self.render_markdown(
                            context,
                            MarkdownPart::Context,
                            window,
                            cx,
                        ));
                    }
                    entry = entry.child(self.render_markdown(
                        &markdown.body,
                        MarkdownPart::Body,
                        window,
                        cx,
                    ));
                    thread = thread.child(entry);
                }
                if *more_comments {
                    thread = thread.child(Label::new(format!("First {ACTIVITY_PAGE_SIZE} comments only; open GitHub for the full thread")).size(LabelSize::Small).color(Color::Warning));
                }
                thread
            }
        };
        if let Some(body) = &view.body {
            row = row.child(self.render_markdown(body, MarkdownPart::Body, window, cx));
        }
        row.into_any_element()
    }
}

impl Render for PullRequestActivity {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.target.pull_request_url(self.number);
        let mut body = v_flex().debug_selector(||"pr-activity".into()).w_full().min_w_0().gap_3().track_focus(&self.focus)
            .child(h_flex().debug_selector(||"pr-activity-controls".into()).w_full().min_w_0().flex_wrap().gap_1()
                .children([(ActivitySection::Timeline,"Timeline"),(ActivitySection::Commits,"Commits"),(ActivitySection::Threads,"Review threads")].into_iter().map(|(section,label)| {
                    div().debug_selector(move || match section { ActivitySection::Timeline => "pr-activity-timeline".into(), ActivitySection::Commits => "pr-activity-commits".into(), ActivitySection::Threads => "pr-activity-threads".into() })
                        .child(Button::new(label,label).tab_index(0_isize).toggle_state(self.section == section).on_click(cx.listener(move |this, _, _, cx| {
                            this.section = section;
                            this.scroll.set_offset(gpui::Point::default());
                            this.refresh(cx);
                        })))
                }))
                .child(Button::new("pr-activity-refresh","Refresh activity").tab_index(0_isize).disabled(self.state.loading).on_click(cx.listener(|this,_,_,cx|this.refresh(cx)))))
            .child(Button::new("pr-activity-github","Open activity on GitHub").tab_index(0_isize).on_click(move |_,_,cx|cx.open_url(&url)))
            .child(Label::new(format!("Read-only snapshot · up to {ACTIVITY_LIMIT} entries, {ACTIVITY_PAGE_SIZE} per page. Thread replies are bounded separately.")).size(LabelSize::Small).color(Color::Muted));
        if let Err(error) = check_access(&self.project, cx) {
            return body.child(Label::new(format!("{error:#}")).color(Color::Error));
        }
        for item in &self.state.items {
            if let Some(view) = self.markdown.get(&item.id) {
                body = body.child(self.render_item(item, view, window, cx));
            }
        }
        if self.state.loading {
            body = body.child(Label::new("Loading activity…").color(Color::Muted));
        } else if self.state.items.is_empty() && self.state.error.is_none() {
            body = body.child(Label::new("No activity in this section.").color(Color::Muted));
        }
        if let Some(error) = &self.state.error {
            body = body.child(Label::new(error.clone()).color(Color::Error));
        }
        if self.state.can_load_more() {
            body = body.child(
                div().debug_selector(|| "pr-activity-more".into()).child(
                    Button::new("pr-activity-more", "Load more")
                        .tab_index(0_isize)
                        .disabled(self.state.loading)
                        .on_click(cx.listener(|this, _, _, cx| this.load(LoadMode::Append, cx))),
                ),
            );
        } else if self.state.cursor.is_some() {
            body = body.child(
                Label::new("Activity page limit reached; open GitHub for the rest.")
                    .color(Color::Muted),
            );
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PreviewRequest, PullRequestDescription};
    use super::*;
    use crate::github_pull_requests::ActivityComment;
    use git::repository::RevisionDiffMode;
    use gpui::{TestAppContext, VisualTestContext, WeakEntity};
    use std::sync::Mutex;

    fn page(id: &str, cursor: Option<&str>) -> ActivityPage {
        ActivityPage {
            head: "1".repeat(40),
            cursor: cursor.map(str::to_owned),
            items: vec![ActivityItem {
                id: id.into(),
                author: "contributor".into(),
                date: "2026-01-01T00:00:00Z".into(),
                body: "**Comment**".into(),
                kind: ActivityKind::Comment,
            }],
        }
    }

    #[test]
    fn test_activity_paging() {
        let mut state = ActivityState::default();
        let generation = state.start(LoadMode::Replace);
        state
            .complete(generation, Ok(page("first", Some("next"))))
            .expect("first page");
        let old = state.start(LoadMode::Append);
        let generation = state.start(LoadMode::Append);
        state
            .complete(old, Ok(page("stale", None)))
            .expect("ignore stale");
        assert_eq!(state.items.len(), 1);
        assert!(state.loading);
        let mut changed = page("changed", None);
        changed.head = "2".repeat(40);
        assert!(state.complete(generation, Ok(changed)).is_err());
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.cursor.as_deref(), Some("next"));
        let generation = state.start(LoadMode::Append);
        assert!(
            state
                .complete(generation, Ok(page("cycle", Some("next"))))
                .is_err()
        );
        let generation = state.start(LoadMode::Append);
        let mut duplicate = page("second", None);
        duplicate.items.push(state.items[0].clone());
        state
            .complete(generation, Ok(duplicate))
            .expect("deduplicate pages");
        assert_eq!(state.items.len(), 2);
        assert!(!state.can_load_more());
        let generation = state.start(LoadMode::Append);
        assert!(
            state
                .complete(generation, Err(anyhow::anyhow!("network failure")))
                .is_err()
        );
        assert!(!state.loading);
        assert_eq!(state.items.len(), 2);
        let generation = state.start(LoadMode::Append);
        let mut large = page("large", None);
        large.items[0].body = "x".repeat(ACTIVITY_TEXT_LIMIT + 1);
        assert!(state.complete(generation, Ok(large)).is_err());
        assert_eq!(state.items.len(), 2);
        state.pages = ACTIVITY_LIMIT / ACTIVITY_PAGE_SIZE;
        state.cursor = Some("remaining".into());
        assert!(!state.can_load_more());
        state.start(LoadMode::Replace);
        assert!(state.items.is_empty());
        assert_eq!(state.pages, 0);
        assert!(state.head.is_none());
    }

    async fn fixture(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<project::git_store::Repository>,
        GitHubRepository,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            git::GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
        });
        let fs = project::FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(util::path!("/repo"), serde_json::json!({".git":{}}))
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
        let target = cx.update(|cx| {
            GitHubRepository::from_remote_url(
                git::GitHostingProviderRegistry::default_global(cx),
                "https://github.com/example/project.git",
            )
            .expect("target")
        });
        (project, repository, target)
    }

    fn trust(project: &Entity<Project>, cx: &mut App) {
        use project::trusted_worktrees::{PathTrust, TrustedWorktrees};
        project::trusted_worktrees::init(Default::default(), cx);
        let store = project.read(cx).worktree_store();
        let paths = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|tree| PathTrust::Worktree(tree.read(cx).id()))
            .collect();
        TrustedWorktrees::try_get_global(cx)
            .expect("trust store")
            .update(cx, |trusted, cx| trusted.trust(&store, paths, cx));
    }

    fn enter(cx: &mut VisualTestContext) {
        cx.refresh().expect("paint focus");
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

    #[gpui::test(iterations = 5)]
    async fn test_activity_ui(cx: &mut TestAppContext) {
        let (project, repository, target) = fixture(cx).await;
        let window = cx.add_window(|_,cx| {
            let mut description = PullRequestDescription::new(project.clone(),Some(repository),WeakEntity::new_invalid(),target,42,cx);
            description.set_detail(serde_json::from_value(serde_json::json!({
                "number":42,"title":"Activity fixture","author":{"login":"contributor"},"isDraft":false,"state":"OPEN","body":"Description",
                "baseRefName":"main","headRefName":"feature","baseRefOid":"1".repeat(40),"headRefOid":"2".repeat(40)
            })).expect("detail"),cx);
            description.error = None;
            description
        });
        let description = window.root(cx).expect("description");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.blur(cx);
            for _ in ["Refresh", "View changes", "GitHub", "Details", "Activity"] {
                window.focus_next(cx);
            }
        });
        enter(&mut cx);
        let activity = description.read_with(&cx, |description, _| {
            description
                .activity
                .clone()
                .expect("keyboard opens activity")
        });
        assert!(
            activity.read_with(&cx, |activity, _| activity.state.error.is_some()),
            "untrusted activity is blocked"
        );
        let mut first = page("comment", Some("next"));
        let commit = "3".repeat(40).parse::<Oid>().expect("commit");
        first.items.insert(
            0,
            ActivityItem {
                id: "commit".into(),
                author: "contributor".into(),
                date: "2026-01-01T00:00:00Z".into(),
                body: String::new(),
                kind: ActivityKind::Commit {
                    oid: commit,
                    title: "Commit title".into(),
                },
            },
        );
        let mut review = page("review", None);
        review.items[0].kind = ActivityKind::Review {
            state: "APPROVED".into(),
        };
        let mut threads = page("thread", None);
        threads.items[0].body.clear();
        threads.items[0].kind = ActivityKind::Thread {
            path: "src/".repeat(40),
            line: Some(12),
            resolved: true,
            outdated: true,
            more_comments: true,
            comments: vec![
                ActivityComment {
                    author: "reviewer".into(),
                    date: "2026-01-01T00:00:00Z".into(),
                    body: "**Review**".into(),
                    hunk: "@@ -1 +1 @@\n-old\n+```".into(),
                },
                ActivityComment {
                    author: "contributor".into(),
                    date: "2026-01-02T00:00:00Z".into(),
                    body: "Reply".into(),
                    hunk: String::new(),
                },
            ],
        };
        let pages = Arc::new(Mutex::new(std::collections::VecDeque::from([
            first, review, threads,
        ])));
        let requests = Arc::new(Mutex::new(Vec::new()));
        activity.update(&mut cx, |activity, cx| {
            let native = activity.on_commit.clone();
            activity.on_commit = Rc::new(move |oid, reveal_file, window, cx| {
                assert_eq!(oid, commit, "the selected commit is not the PR head");
                native(oid, reveal_file, window, cx);
            });
            let requests = requests.clone();
            activity.load_page = Arc::new(move |target, number, section, cursor| {
                assert_eq!(target.slug(), "example/project");
                assert_eq!(number, 42);
                requests.lock().expect("requests").push((section, cursor));
                let page = pages
                    .lock()
                    .expect("pages")
                    .pop_front()
                    .expect("expected request");
                Box::pin(async move { Ok(page) })
            });
            trust(&project, cx);
            activity.refresh(cx);
        });
        cx.run_until_parked();
        for width in [320., 1200.] {
            cx.simulate_resize(gpui::size(px(width), px(1100.)));
            cx.refresh().expect("activity layout");
            cx.run_until_parked();
            let bounds = cx
                .debug_bounds("pr-activity-controls")
                .expect("activity controls");
            assert!(bounds.left() >= px(0.) && bounds.right() <= px(width));
            let changes = cx
                .debug_bounds("pr-commit-changes")
                .expect("per-commit changes");
            assert!(changes.right() <= px(width));
        }
        assert!(
            cx.debug_bounds("pr-markdown").is_none(),
            "description and activity do not share the body"
        );
        let changes = cx.debug_bounds("pr-commit-changes").expect("commit action");
        cx.simulate_click(changes.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        description.read_with(&cx, |description, _| {
            assert!(
                description
                    .diff_error
                    .as_deref()
                    .is_some_and(|error| error.contains("Commit snapshots are not supported")),
                "row dispatches through the native commit facade, not GitHub"
            )
        });
        let more = cx.debug_bounds("pr-activity-more").expect("load next page");
        cx.simulate_click(more.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        activity.read_with(&cx, |activity, _| assert_eq!(activity.state.items.len(), 3));
        let threads = cx
            .debug_bounds("pr-activity-threads")
            .expect("review threads switch");
        cx.simulate_click(threads.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.refresh().expect("thread layout");
        assert!(cx.debug_bounds("pr-review-comment-0").is_some());
        assert!(cx.debug_bounds("pr-review-comment-1").is_some());
        activity.read_with(&cx, |activity, cx| {
            let view = activity.markdown.get("thread").expect("thread Markdown");
            assert_eq!(view.comments[1].body.read(cx).source().as_ref(), "Reply");
            assert!(
                view.comments[0]
                    .context
                    .as_ref()
                    .expect("inline context")
                    .read(cx)
                    .source()
                    .starts_with("````diff\n")
            );
            assert_eq!(activity.state.items.len(), 1);
        });
        assert_eq!(
            *requests.lock().expect("requests"),
            vec![
                (ActivitySection::Timeline, None),
                (ActivitySection::Timeline, Some("next".into())),
                (ActivitySection::Threads, None)
            ]
        );
        let toggle = cx
            .debug_bounds("pr-activity-toggle")
            .expect("return to description");
        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("pr-markdown").is_some());
        assert!(cx.debug_bounds("pr-activity").is_none());
        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("pr-review-comment-1").is_some(),
            "activity retained in this view"
        );
        assert_eq!(
            requests.lock().expect("requests").len(),
            3,
            "no polling/reload merely switching description"
        );
    }

    fn selected_word(
        cx: &mut VisualTestContext,
        markdown: &Entity<Markdown>,
        bounds: gpui::Bounds<gpui::Pixels>,
    ) -> String {
        let position = bounds.origin + gpui::point(px(20.), px(30.));
        cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
        cx.simulate_event(gpui::MouseDownEvent {
            position,
            button: gpui::MouseButton::Left,
            click_count: 2,
            modifiers: gpui::Modifiers::default(),
            first_mouse: false,
        });
        cx.simulate_mouse_up(
            position,
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_down(
            position,
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            position,
            gpui::MouseButton::Right,
            gpui::Modifiers::default(),
        );
        markdown.read_with(cx, |markdown, _| {
            markdown
                .context_menu_selected_text()
                .expect("selected code word")
                .to_string()
        })
    }

    #[gpui::test(iterations = 5)]
    async fn test_activity_review_scroll(cx: &mut TestAppContext) {
        let (project, _, target) = fixture(cx).await;
        cx.update(|cx| {
            trust(&project, cx);
            cx.set_reduce_motion(true);
        });
        let context = (0..200)
            .map(|index| format!("context{index:03} "))
            .collect::<String>();
        let body = format!(
            "```text\n{}\n```",
            (0..200)
                .map(|index| format!("body{index:03} "))
                .collect::<String>()
        );
        let window = cx.add_window(|_, cx| {
            let mut activity = PullRequestActivity::new(
                project,
                target,
                42,
                ScrollHandle::new(),
                Rc::new(|_, _, _, _| {}),
                Rc::new(|_, _| Task::ready(Ok(Vec::new()))),
                cx,
            );
            let mut page = page("scroll-thread", None);
            page.items[0].body.clear();
            page.items[0].kind = ActivityKind::Thread {
                path: "file.rs".into(),
                line: Some(1),
                resolved: false,
                outdated: false,
                more_comments: false,
                comments: vec![ActivityComment {
                    author: "reviewer".into(),
                    date: String::new(),
                    body,
                    hunk: context,
                }],
            };
            let generation = activity.state.start(LoadMode::Replace);
            activity
                .state
                .complete(generation, Ok(page))
                .expect("thread");
            activity.prepare_markdown(cx);
            activity
        });
        let activity = window.root(cx).expect("activity");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        cx.run_until_parked();
        cx.refresh().expect("scrollbar fixture");
        cx.run_until_parked();
        let (context, body) = activity.read_with(&cx, |activity, _| {
            let comment = activity
                .markdown
                .get("scroll-thread")
                .expect("thread")
                .comments
                .first()
                .expect("comment");
            (
                comment.context.clone().expect("context"),
                comment.body.clone(),
            )
        });
        let context_bounds = cx
            .debug_bounds("pr-review-context")
            .expect("context bounds");
        let body_bounds = cx.debug_bounds("pr-review-body").expect("body bounds");
        let context_before = selected_word(&mut cx, &context, context_bounds);
        let body_before = selected_word(&mut cx, &body, body_bounds);
        assert!(context_before.starts_with("context"));
        assert!(body_before.starts_with("body"));
        const SCROLL_DISTANCE: f32 = 400.;
        for delta in [-SCROLL_DISTANCE, SCROLL_DISTANCE] {
            cx.simulate_event(gpui::ScrollEvent {
                position: body_bounds.origin + gpui::point(px(20.), px(30.)),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(delta), px(0.))),
                modifiers: gpui::Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
                source: gpui::ScrollSource::Pointer,
            });
            cx.refresh().expect("wheel scroll paint");
            cx.run_until_parked();
            let word = selected_word(&mut cx, &body, body_bounds);
            if delta < 0. {
                assert_ne!(word, body_before, "fixture has horizontal overflow");
            } else {
                assert_eq!(word, body_before, "restore initial horizontal position");
            }
        }
        let track = gpui::point(body_bounds.center().x, body_bounds.bottom() - px(8.));
        cx.simulate_mouse_move(track, None, gpui::Modifiers::default());
        cx.refresh().expect("paint scrollbar hover");
        cx.run_until_parked();
        cx.simulate_click(track, gpui::Modifiers::default());
        cx.refresh().expect("paint horizontal scroll");
        cx.run_until_parked();
        let body_after = selected_word(&mut cx, &body, body_bounds);
        let context_after = selected_word(&mut cx, &context, context_bounds);
        assert_ne!(
            body_after, body_before,
            "body scrollbar must move body code"
        );
        assert_eq!(
            context_after, context_before,
            "body scrollbar must not move review context"
        );
    }

    struct PreviewFixture {
        window: gpui::WindowHandle<workspace::MultiWorkspace>,
        workspace: Entity<workspace::Workspace>,
        description: Entity<PullRequestDescription>,
        repository: Entity<project::git_store::Repository>,
        project: Entity<Project>,
    }

    async fn preview_fixture(cx: &mut TestAppContext) -> PreviewFixture {
        let (project, repository, target) = fixture(cx).await;
        cx.update(|cx| {
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
        let window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = window
            .read_with(cx, |root, _| root.workspace().clone())
            .expect("workspace");
        let description = cx.new(|cx| {
            PullRequestDescription::new(
                project.clone(),
                Some(repository.clone()),
                workspace.downgrade(),
                target,
                42,
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
                    )
                })
            })
            .expect("description pane");
        PreviewFixture {
            window,
            workspace,
            description,
            repository,
            project,
        }
    }

    fn root_snapshot() -> project::git_store::RevisionDiff {
        const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        project::git_store::RevisionDiff {
            base: EMPTY_TREE_SHA.parse().expect("empty tree"),
            head: "3".repeat(40).parse().expect("commit"),
            files: vec![project::git_store::CommitFile {
                path: git::repository::repo_path("root.txt"),
                old_text: None,
                new_text: Some("root content\n".into()),
                is_binary: false,
            }],
        }
    }

    #[gpui::test(iterations = 5)]
    async fn test_activity_review_cancel(cx: &mut TestAppContext) {
        let fixture = preview_fixture(cx).await;
        fixture
            .description
            .update(cx, |description, cx| description.toggle_activity(cx));
        let activity = fixture.description.read_with(cx, |description, _| {
            description.activity.clone().expect("activity")
        });
        cx.update(|cx| trust(&fixture.project, cx));
        activity.update(cx, |activity, _| {
            activity.load_page =
                Arc::new(|_, _, _, _| Box::pin(async { Ok(page("refreshed", None)) }))
        });
        let (sender, receiver) = futures::channel::oneshot::channel();
        let response = cx.background_spawn(async {
            receiver
                .await
                .map_err(|_| anyhow::anyhow!("fixture closed"))?
        });
        fixture
            .window
            .update(cx, |_, window, cx| {
                fixture.description.update(cx, |description, cx| {
                    description.show_diff(
                        response,
                        fixture.repository.clone(),
                        "Commit preview".into(),
                        RevisionDiffMode::Direct,
                        None,
                        window,
                        cx,
                    )
                })
            })
            .expect("pending native preview");
        cx.run_until_parked();
        assert!(
            fixture
                .description
                .read_with(cx, |description, _| description.diff_loading)
        );
        let generation = fixture
            .description
            .read_with(cx, |description, _| description.preview_generation);
        activity.update(cx, |activity, cx| activity.refresh(cx));
        cx.run_until_parked();
        assert!(
            !fixture
                .description
                .read_with(cx, |description, _| description.diff_loading),
            "activity replacement must invalidate parent-owned preview"
        );
        assert!(
            sender.send(Ok(root_snapshot())).is_err(),
            "cancelled preview drops its transport"
        );
        fixture
            .window
            .update(cx, |_, window, cx| {
                PullRequestDescription::finish_diff(
                    &fixture.description.downgrade(),
                    Ok(root_snapshot()),
                    PreviewRequest {
                        generation,
                        mode: RevisionDiffMode::Direct,
                        repository: fixture.repository.clone(),
                        title: "Late obsolete commit".into(),
                        reveal_file: None,
                    },
                    window,
                    cx,
                );
            })
            .expect("late completion");
        fixture.workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .active_item_as::<PullRequestDescription>(cx)
                    .is_some()
            )
        });
    }

    #[gpui::test(iterations = 5)]
    async fn test_activity_review_root(cx: &mut TestAppContext) {
        let fixture = preview_fixture(cx).await;
        cx.update(|cx| trust(&fixture.project, cx));
        fixture
            .window
            .update(cx, |_, window, cx| {
                fixture.description.update(cx, |description, cx| {
                    description.show_diff(
                        Task::ready(Ok(root_snapshot())),
                        fixture.repository.clone(),
                        "Root commit".into(),
                        RevisionDiffMode::Direct,
                        None,
                        window,
                        cx,
                    )
                })
            })
            .expect("root preview");
        cx.run_until_parked();
        fixture.workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .active_item_as::<crate::revision_diff::RevisionDiffView>(cx)
                    .is_some()
            )
        });
        let mut cx = VisualTestContext::from_window(fixture.window.into(), cx);
        cx.refresh().expect("root preview header");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("revision-base-direct").is_some(),
            "root empty tree is not a merge base"
        );
        assert!(cx.debug_bounds("revision-base-merge").is_none());
    }

    #[gpui::test(iterations = 5)]
    async fn test_activity_requests(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (project, _, target) = fixture(cx).await;
        let activity = cx.new(|cx| {
            PullRequestActivity::new(
                project.clone(),
                target,
                42,
                ScrollHandle::new(),
                Rc::new(|_, _, _, _| {}),
                Rc::new(|_, _| Task::ready(Ok(Vec::new()))),
                cx,
            )
        });
        let requests = Arc::new(AtomicUsize::new(0));
        activity.update(cx, |activity, cx| {
            let requests = requests.clone();
            activity.load_page = Arc::new(move |_, _, _, _| {
                let index = requests.fetch_add(1, Ordering::SeqCst);
                if index == 1 {
                    Box::pin(async { Ok(page("replacement", None)) })
                } else {
                    Box::pin(futures::future::pending())
                }
            });
            activity.refresh(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "no untrusted network request"
        );
        cx.update(|cx| trust(&project, cx));
        activity.update(cx, |activity, cx| activity.refresh(cx));
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        activity.update(cx, |activity, cx| activity.refresh(cx));
        cx.run_until_parked();
        activity.read_with(cx, |activity, _| {
            assert_eq!(activity.state.items[0].id, "replacement")
        });
        activity.update(cx, |activity, cx| activity.refresh(cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(ACTIVITY_TIMEOUT + Duration::from_secs(1));
        cx.run_until_parked();
        activity.read_with(cx, |activity, _| {
            assert!(!activity.state.loading);
            assert!(
                activity
                    .state
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("timed out"))
            );
            assert!(activity.state.items.is_empty());
        });

        let (sender, receiver) = futures::channel::oneshot::channel();
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        activity.update(cx, |activity, cx| {
            activity.load_page = Arc::new(move |_, _, _, _| {
                let receiver = receiver
                    .lock()
                    .expect("response")
                    .take()
                    .expect("one request");
                Box::pin(async move { receiver.await.expect("test response") })
            });
            activity.refresh(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            use project::trusted_worktrees::{PathTrust, TrustedWorktrees};
            let store = project.read(cx).worktree_store();
            let paths = project
                .read(cx)
                .visible_worktrees(cx)
                .map(|tree| PathTrust::Worktree(tree.read(cx).id()))
                .collect();
            TrustedWorktrees::try_get_global(cx)
                .expect("trust store")
                .update(cx, |trusted, cx| {
                    trusted.restrict(store.downgrade(), paths, cx)
                });
        });
        sender
            .send(Ok(page("private-after-revoke", None)))
            .expect("pending request");
        cx.run_until_parked();
        activity.read_with(cx, |activity, _| {
            assert!(
                activity.state.items.is_empty(),
                "revoked trust rejects arriving content"
            );
            assert!(
                activity
                    .state
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("Trust"))
            );
        });

        struct RequestDrop(Arc<AtomicUsize>);
        impl Drop for RequestDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        cx.update(|cx| trust(&project, cx));
        activity.update(cx, |activity, cx| {
            let dropped = dropped.clone();
            activity.load_page = Arc::new(move |_, _, _, _| {
                let dropped = dropped.clone();
                Box::pin(async move {
                    let _request = RequestDrop(dropped);
                    futures::future::pending::<Result<ActivityPage>>().await
                })
            });
            activity.refresh(cx);
        });
        cx.run_until_parked();
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        activity.read_with(cx, |activity, _| {
            assert!(activity.state.loading, "closure fixture started a request")
        });
        drop(activity);
        // App updates release dropped entities before their owned tasks can be cancelled.
        cx.update(|_| {});
        cx.run_until_parked();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "view closure drops the API transport future"
        );
    }
}
