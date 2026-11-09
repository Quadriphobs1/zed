use std::rc::Rc;

use git::repository::{CommitFileStatus, RepoPath};
use git::status::{DiffStat, FileStatus, StatusCode, TrackedStatus};
use gpui::{AnyElement, App, ClickEvent, ElementId, SharedString, Window};
use project::git_store::CommitFile;
use ui::{Color, Label, LabelSize, prelude::*};

use crate::git_status_icon;

/// A single file touched by a commit, summarized for list rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitFileSummary {
    pub(crate) path: RepoPath,
    pub(crate) status: CommitFileStatus,
    pub(crate) stat: DiffStat,
    pub(crate) is_binary: bool,
}

/// The state of a lazily loaded file list for one commit.
pub(crate) enum CommitFileList {
    Loading,
    Loaded(Rc<[CommitFileSummary]>),
    Error(SharedString),
}

/// Computes per-file line counts. Run this off the main thread: diffing the
/// full contents of every file in a commit is expensive for large commits.
pub(crate) fn summarize_commit_files(files: &[CommitFile]) -> Vec<CommitFileSummary> {
    files
        .iter()
        .map(|file| {
            let stat = if file.is_binary {
                DiffStat::default()
            } else {
                let old_text = file.old_text.as_deref().unwrap_or_default();
                let new_text = file.new_text.as_deref().unwrap_or_default();
                let mut stat = DiffStat::default();
                for (old_rows, new_rows) in language::line_diff(old_text, new_text) {
                    stat.deleted += old_rows.end.saturating_sub(old_rows.start);
                    stat.added += new_rows.end.saturating_sub(new_rows.start);
                }
                stat
            };
            CommitFileSummary {
                path: file.path.clone(),
                status: file.status(),
                stat,
                is_binary: file.is_binary,
            }
        })
        .collect()
}

fn file_status(status: CommitFileStatus) -> FileStatus {
    let index_status = match status {
        CommitFileStatus::Added => StatusCode::Added,
        CommitFileStatus::Modified => StatusCode::Modified,
        CommitFileStatus::Deleted => StatusCode::Deleted,
    };
    FileStatus::Tracked(TrackedStatus {
        index_status,
        worktree_status: StatusCode::Unmodified,
    })
}

/// Renders one changed file on a single line: status, name, directory and
/// line counts. `uniform_list` sizes every row from its first item, so this
/// row fills the height it is given rather than defining it.
pub(crate) fn render_commit_file(
    id: impl Into<ElementId>,
    file: &CommitFileSummary,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let file_name: SharedString = file
        .path
        .file_name()
        .unwrap_or_else(|| file.path.as_unix_str())
        .to_string()
        .into();
    let directory: SharedString = file
        .path
        .parent()
        .map(|parent| parent.as_unix_str().to_string())
        .unwrap_or_default()
        .into();
    let full_path: SharedString = file.path.as_unix_str().to_string().into();

    h_flex()
        .id(id.into())
        .cursor_pointer()
        .w_full()
        .min_w_0()
        .h_full()
        .items_center()
        .py_0p5()
        .pr_2()
        .pl_6()
        .gap_1p5()
        .hover(|style| style.bg(cx.theme().colors().element_hover))
        .tooltip(ui::Tooltip::text(full_path))
        .child(git_status_icon(file_status(file.status)))
        .child(Label::new(file_name).truncate())
        .when(!directory.is_empty(), |this| {
            this.child(
                Label::new(directory)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
        })
        .child(div().flex_1())
        .child(render_diff_stat(file))
        .on_click(move |event, window, cx| on_click(event, window, cx))
        .into_any_element()
}

fn render_diff_stat(file: &CommitFileSummary) -> AnyElement {
    if file.is_binary {
        return Label::new("binary")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element();
    }

    h_flex()
        .flex_none()
        .gap_1()
        .child(
            Label::new(format!("+{}", file.stat.added))
                .size(LabelSize::Small)
                .color(Color::Created),
        )
        .child(
            Label::new(format!("−{}", file.stat.deleted))
                .size(LabelSize::Small)
                .color(Color::Deleted),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::RepoPath;

    fn commit_file(path: &str, old_text: Option<&str>, new_text: Option<&str>) -> CommitFile {
        CommitFile {
            path: RepoPath::from_rel_path(util::rel_path::rel_path(path)),
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            is_binary: false,
            old_bytes: None,
            new_bytes: None,
        }
    }

    #[test]
    fn test_summarize_commit_files() {
        let summaries = summarize_commit_files(&[
            commit_file("src/main.rs", Some("a\nb\nc\n"), Some("a\nB\nc\nd\n")),
            commit_file("README.md", None, Some("one\ntwo\n")),
            commit_file("old.txt", Some("gone\n"), None),
        ]);

        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].status, CommitFileStatus::Modified);
        assert_eq!(
            summaries[0].stat,
            DiffStat {
                added: 2,
                deleted: 1
            }
        );
        assert_eq!(summaries[1].status, CommitFileStatus::Added);
        assert_eq!(
            summaries[1].stat,
            DiffStat {
                added: 2,
                deleted: 0
            }
        );
        assert_eq!(summaries[2].status, CommitFileStatus::Deleted);
        assert_eq!(
            summaries[2].stat,
            DiffStat {
                added: 0,
                deleted: 1
            }
        );
    }
}
