//! Builds a high level, code-free picture of a change: which files and symbols
//! it touches, and how they relate. The structure is computed locally from the
//! diff so that it is deterministic; a language model, when available, only
//! narrates the structure it is given.

use anyhow::Result;
use git::repository::{CommitFileStatus, RepoPath};
use git::status::DiffStat;
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};
use language::Rope;
use language::{Buffer, LanguageRegistry};
use project::git_store::CommitFile;
use std::fmt::Write as _;
use std::sync::Arc;

use crate::commit_file_list::summarize_commit_files;

/// How many files and symbols a generated diagram may contain before it is
/// truncated. Mermaid graphs become unreadable, and slow to rasterize, long
/// before a large pull request runs out of files.
const MAX_DIAGRAM_FILES: usize = 20;
const MAX_DIAGRAM_SYMBOLS_PER_FILE: usize = 6;
/// Keeps the model prompt roughly constant in size regardless of diff size.
const MAX_PROMPT_FILES: usize = 60;
const MAX_PROMPT_SYMBOLS_PER_FILE: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SymbolChange {
    Added,
    Removed,
    Modified,
}

impl SymbolChange {
    fn marker(self) -> &'static str {
        match self {
            SymbolChange::Added => "+",
            SymbolChange::Removed => "-",
            SymbolChange::Modified => "~",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedSymbol {
    pub(crate) name: SharedString,
    pub(crate) change: SymbolChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileOverview {
    pub(crate) path: RepoPath,
    pub(crate) status: CommitFileStatus,
    pub(crate) stat: DiffStat,
    pub(crate) is_binary: bool,
    /// Empty when the file has no outline support, or when only comments and
    /// whitespace between symbols changed.
    pub(crate) symbols: Vec<ChangedSymbol>,
}

/// One caller of a changed symbol, as reported by a language server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallEdge {
    pub(crate) caller: SharedString,
    pub(crate) caller_path: Option<SharedString>,
    pub(crate) callee: SharedString,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangeOverview {
    pub(crate) title: SharedString,
    pub(crate) files: Vec<FileOverview>,
    /// Newest first, as GitHub reports them.
    pub(crate) commits: Vec<(SharedString, SharedString)>,
    /// Who calls the changed symbols. Only available for changes whose files
    /// are checked out locally and whose language server answers.
    pub(crate) call_edges: Vec<CallEdge>,
}

/// Extracts the structure of a change. Parsing happens off the main thread, so
/// this is safe to call for a whole pull request.
pub(crate) fn build_change_overview(
    title: SharedString,
    files: Vec<CommitFile>,
    commits: Vec<(SharedString, SharedString)>,
    languages: Option<Arc<LanguageRegistry>>,
    cx: &mut App,
) -> Task<Result<ChangeOverview>> {
    let summaries = summarize_commit_files(&files);
    let language_requests = files
        .iter()
        .map(|file| {
            let languages = languages.as_ref()?;
            let language_id = languages.language_for_file_path(file.path.as_std_path())?;
            Some(languages.load_language(language_id))
        })
        .collect::<Vec<_>>();

    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut file_languages = Vec::with_capacity(language_requests.len());
        for request in language_requests {
            // A missing grammar only costs the file its symbol list.
            let language = match request {
                Some(request) => request.await.ok().and_then(|language| language.ok()),
                None => None,
            };
            file_languages.push(language);
        }

        let snapshots = cx.update(|cx| {
            files
                .iter()
                .zip(&file_languages)
                .map(|(file, language)| {
                    let mut build = |text: Option<&String>| {
                        text.map(|text| {
                            Buffer::build_snapshot(
                                Rope::from(text.as_str()),
                                language.clone(),
                                languages.clone(),
                                None,
                                cx,
                            )
                        })
                    };
                    let old = build(file.old_text.as_ref());
                    let new = build(file.new_text.as_ref());
                    (old, new)
                })
                .collect::<Vec<_>>()
        });

        cx.background_spawn(async move {
            let mut overview = ChangeOverview {
                title,
                files: Vec::with_capacity(snapshots.len()),
                commits,
                call_edges: Vec::new(),
            };
            for (summary, (old_snapshot, new_snapshot)) in summaries.into_iter().zip(snapshots) {
                let old_symbols = match old_snapshot {
                    Some(snapshot) => outline_symbols(&snapshot.await),
                    None => Vec::new(),
                };
                let new_symbols = match new_snapshot {
                    Some(snapshot) => outline_symbols(&snapshot.await),
                    None => Vec::new(),
                };
                overview.files.push(FileOverview {
                    path: summary.path,
                    status: summary.status,
                    stat: summary.stat,
                    is_binary: summary.is_binary,
                    symbols: changed_symbols(&old_symbols, &new_symbols),
                });
            }
            Ok(overview)
        })
        .await
    })
}

fn outline_symbols(snapshot: &language::BufferSnapshot) -> Vec<(SharedString, String)> {
    snapshot
        .outline(None)
        .items
        .into_iter()
        // Nested items are locals and fields; a change summary is about the
        // declarations that other code can reach.
        .filter(|item| item.depth == 0)
        .map(|item| {
            let body = snapshot.text_for_range(item.range).collect::<String>();
            (item.text, body)
        })
        .collect()
}

fn changed_symbols(
    old_symbols: &[(SharedString, String)],
    new_symbols: &[(SharedString, String)],
) -> Vec<ChangedSymbol> {
    let mut changes = Vec::new();
    for (name, body) in new_symbols {
        match old_symbols.iter().find(|(old_name, _)| old_name == name) {
            None => changes.push(ChangedSymbol {
                name: name.clone(),
                change: SymbolChange::Added,
            }),
            Some((_, old_body)) if old_body != body => changes.push(ChangedSymbol {
                name: name.clone(),
                change: SymbolChange::Modified,
            }),
            Some(_) => {}
        }
    }
    for (name, _) in old_symbols {
        if !new_symbols.iter().any(|(new_name, _)| new_name == name) {
            changes.push(ChangedSymbol {
                name: name.clone(),
                change: SymbolChange::Removed,
            });
        }
    }
    changes
}

/// How much language server work a single overview may trigger.
const MAX_CALL_GRAPH_FILES: usize = 10;
const MAX_CALL_GRAPH_SYMBOLS: usize = 12;
const MAX_CALLERS_PER_SYMBOL: usize = 6;

/// Asks the language servers which code calls the symbols this change touches.
/// Only the working copy can be queried, so this is for local revisions; files
/// that are absent, unsupported or still indexing simply contribute no edges.
pub(crate) fn collect_call_edges(
    overview: &ChangeOverview,
    project: Entity<project::Project>,
    repository: Entity<project::git_store::Repository>,
    cx: &mut App,
) -> Task<Vec<CallEdge>> {
    let targets = overview
        .files
        .iter()
        .filter(|file| file.status != CommitFileStatus::Deleted && !file.symbols.is_empty())
        .take(MAX_CALL_GRAPH_FILES)
        .map(|file| {
            let symbols = file
                .symbols
                .iter()
                .filter(|symbol| symbol.change != SymbolChange::Removed)
                .map(|symbol| symbol.name.clone())
                .collect::<Vec<_>>();
            (file.path.clone(), symbols)
        })
        .collect::<Vec<_>>();

    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut edges: Vec<CallEdge> = Vec::new();
        let mut budget = MAX_CALL_GRAPH_SYMBOLS;
        for (path, symbols) in targets {
            if budget == 0 {
                break;
            }
            let Some(project_path) = repository.read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&path, cx)
            }) else {
                continue;
            };
            let buffer = cx.update(|cx| {
                project.update(cx, |project, cx| project.open_buffer(project_path, cx))
            });
            let Ok(buffer) = buffer.await else {
                continue;
            };

            let positions = buffer.read_with(cx, |buffer, _| {
                let snapshot = buffer.snapshot();
                snapshot
                    .outline(None)
                    .items
                    .into_iter()
                    .filter(|item| item.depth == 0 && symbols.contains(&item.text))
                    .map(|item| (item.text, item.selection_range.start))
                    .collect::<Vec<_>>()
            });

            for (name, position) in positions {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let prepare = cx.update(|cx| {
                    project.update(cx, |project, cx| {
                        project.prepare_call_hierarchy(&buffer, position, cx)
                    })
                });
                let Some(item) = prepare
                    .await
                    .ok()
                    .flatten()
                    .and_then(|items| items.into_iter().next())
                else {
                    continue;
                };
                let calls = cx.update(|cx| {
                    project.update(cx, |project, cx| project.incoming_calls(item, cx))
                });
                let Some(calls) = calls.await.ok().flatten() else {
                    continue;
                };
                for call in calls.into_iter().take(MAX_CALLERS_PER_SYMBOL) {
                    let caller_path = call.from.buffer.read_with(cx, |buffer, _| {
                        buffer
                            .file()
                            .map(|file| SharedString::from(file.path().as_unix_str().to_string()))
                    });
                    let edge = CallEdge {
                        caller: call.from.name.into(),
                        caller_path,
                        callee: name.clone(),
                    };
                    if !edges.contains(&edge) {
                        edges.push(edge);
                    }
                }
            }
        }
        edges
    })
}

impl ChangeOverview {
    fn totals(&self) -> (u32, u32) {
        self.files.iter().fold((0, 0), |(added, deleted), file| {
            (added + file.stat.added, deleted + file.stat.deleted)
        })
    }

    /// Markdown with embedded mermaid diagrams, rendered without calling out to
    /// a model.
    pub(crate) fn to_markdown(&self) -> String {
        let (added, deleted) = self.totals();
        let mut markdown = format!(
            "## {}\n\n{} file(s) changed, +{added} −{deleted}\n\n",
            self.title,
            self.files.len()
        );
        markdown.push_str(&self.impact_diagram());
        markdown.push_str(&self.call_graph_diagram());
        if !self.commits.is_empty() {
            markdown.push_str(&self.commit_diagram());
        }
        markdown.push_str(&self.file_table());
        markdown
    }

    fn impact_diagram(&self) -> String {
        let mut diagram = String::from("### Impact map\n\n```mermaid\nflowchart LR\n");
        let mut directories: Vec<&str> = Vec::new();
        for file in self.files.iter().take(MAX_DIAGRAM_FILES) {
            let directory = file_directory(file);
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }

        for (directory_index, directory) in directories.iter().enumerate() {
            let _ = writeln!(
                diagram,
                "  subgraph d{directory_index}[\"{}\"]",
                escape(directory)
            );
            for (file_index, file) in self.files.iter().take(MAX_DIAGRAM_FILES).enumerate() {
                if file_directory(file) != *directory {
                    continue;
                }
                let name = file
                    .path
                    .file_name()
                    .unwrap_or_else(|| file.path.as_unix_str());
                // Symbols live inside the file's node: a node per symbol turns
                // even a small change into a diagram taller than the window.
                let mut label = format!(
                    "{} {}<br/>+{} \u{2212}{}",
                    status_marker(file.status),
                    escape(name),
                    file.stat.added,
                    file.stat.deleted
                );
                for symbol in file.symbols.iter().take(MAX_DIAGRAM_SYMBOLS_PER_FILE) {
                    let _ = write!(
                        label,
                        "<br/>{} {}",
                        symbol.change.marker(),
                        escape(&truncate(&symbol.name, 48))
                    );
                }
                let hidden = file
                    .symbols
                    .len()
                    .saturating_sub(MAX_DIAGRAM_SYMBOLS_PER_FILE);
                if hidden > 0 {
                    let _ = write!(label, "<br/>\u{2026} {hidden} more");
                }
                let _ = writeln!(diagram, "    f{file_index}[\"{label}\"]");
            }
            diagram.push_str("  end\n");
        }

        let hidden_files = self.files.len().saturating_sub(MAX_DIAGRAM_FILES);
        if hidden_files > 0 {
            let _ = writeln!(diagram, "  more[\"\u{2026} {hidden_files} more file(s)\"]");
        }
        diagram.push_str("```\n\n");
        diagram
    }

    /// Draws who calls the changed symbols, which is the part of a change that
    /// a file list cannot show.
    fn call_graph_diagram(&self) -> String {
        if self.call_edges.is_empty() {
            return String::new();
        }
        let mut diagram = String::from("### Callers of changed code\n\n```mermaid\nflowchart LR\n");
        let mut callers: Vec<&CallEdge> = Vec::new();
        for edge in &self.call_edges {
            if !callers.iter().any(|existing| {
                existing.caller == edge.caller && existing.caller_path == edge.caller_path
            }) {
                callers.push(edge);
            }
        }
        let mut callees: Vec<&SharedString> = Vec::new();
        for edge in &self.call_edges {
            if !callees.contains(&&edge.callee) {
                callees.push(&edge.callee);
            }
        }

        for (index, callee) in callees.iter().enumerate() {
            let _ = writeln!(diagram, "  t{index}[\"{}\"]", escape(callee));
        }
        for (caller_index, caller) in callers.iter().enumerate() {
            let label = match &caller.caller_path {
                Some(path) => format!("{} ({})", caller.caller, truncate(path, 40)),
                None => caller.caller.to_string(),
            };
            let _ = writeln!(diagram, "  c{caller_index}[\"{}\"]", escape(&label));
        }
        for edge in &self.call_edges {
            let Some(caller_index) = callers.iter().position(|existing| {
                existing.caller == edge.caller && existing.caller_path == edge.caller_path
            }) else {
                continue;
            };
            let Some(callee_index) = callees.iter().position(|callee| **callee == edge.callee)
            else {
                continue;
            };
            let _ = writeln!(diagram, "  c{caller_index} --> t{callee_index}");
        }
        diagram.push_str("```\n\n");
        diagram
    }

    fn commit_diagram(&self) -> String {
        let mut diagram = String::from("### Commits\n\n```mermaid\ngitGraph\n");
        // gitGraph draws oldest first, while GitHub reports newest first.
        for (sha, title) in self.commits.iter().rev() {
            let _ = writeln!(
                diagram,
                "  commit id: \"{} {}\"",
                escape(sha),
                escape(&truncate(title, 48))
            );
        }
        diagram.push_str("```\n\n");
        diagram
    }

    fn file_table(&self) -> String {
        let mut table =
            String::from("### Files\n\n| File | Change | Symbols |\n| --- | --- | --- |\n");
        for file in &self.files {
            let symbols = if file.is_binary {
                "binary".to_string()
            } else if file.symbols.is_empty() {
                "—".to_string()
            } else {
                file.symbols
                    .iter()
                    .take(MAX_PROMPT_SYMBOLS_PER_FILE)
                    .map(|symbol| format!("{} {}", symbol.change.marker(), symbol.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let _ = writeln!(
                table,
                "| `{}` | {} +{} −{} | {} |",
                file.path.as_unix_str(),
                status_marker(file.status),
                file.stat.added,
                file.stat.deleted,
                symbols
            );
        }
        table.push('\n');
        table
    }

    /// A compact, code-free description of the change for a language model.
    /// Sending structure rather than the diff keeps the request small and stops
    /// the model from inventing symbols that the change does not touch.
    pub(crate) fn to_prompt(&self) -> String {
        let (added, deleted) = self.totals();
        let mut prompt = format!(
            "Change: {}\nFiles changed: {}, +{added} −{deleted}\n",
            self.title,
            self.files.len()
        );
        if !self.commits.is_empty() {
            prompt.push_str("Commits (newest first):\n");
            for (sha, title) in &self.commits {
                let _ = writeln!(prompt, "- {sha} {title}");
            }
        }
        prompt.push_str("Changed files and symbols (+ added, - removed, ~ modified):\n");
        for file in self.files.iter().take(MAX_PROMPT_FILES) {
            let _ = writeln!(
                prompt,
                "- {} ({} +{} −{})",
                file.path.as_unix_str(),
                status_marker(file.status),
                file.stat.added,
                file.stat.deleted
            );
            for symbol in file.symbols.iter().take(MAX_PROMPT_SYMBOLS_PER_FILE) {
                let _ = writeln!(prompt, "    {} {}", symbol.change.marker(), symbol.name);
            }
            let hidden = file
                .symbols
                .len()
                .saturating_sub(MAX_PROMPT_SYMBOLS_PER_FILE);
            if hidden > 0 {
                let _ = writeln!(prompt, "    … {hidden} more symbol(s)");
            }
        }
        let hidden_files = self.files.len().saturating_sub(MAX_PROMPT_FILES);
        if hidden_files > 0 {
            let _ = writeln!(prompt, "- … {hidden_files} more file(s)");
        }
        if !self.call_edges.is_empty() {
            prompt.push_str("Existing callers of the changed symbols:\n");
            for edge in &self.call_edges {
                match &edge.caller_path {
                    Some(path) => {
                        let _ =
                            writeln!(prompt, "- {} ({path}) calls {}", edge.caller, edge.callee);
                    }
                    None => {
                        let _ = writeln!(prompt, "- {} calls {}", edge.caller, edge.callee);
                    }
                }
            }
        }
        prompt
    }
}

/// The instructions paired with [`ChangeOverview::to_prompt`].
pub(crate) const OVERVIEW_INSTRUCTIONS: &str = "\
You are explaining a code change to a reviewer who has not read the diff yet.
You are given only the structure of the change: files, line counts and the \
symbols that were added, removed or modified. Do not invent functions, files \
or behaviour that is not listed.

Reply in Markdown with these sections, in order:

1. `## Summary` — at most four sentences on what the change does and why the \
touched symbols suggest that purpose.
2. `## Before and after` — one `mermaid` code block with a `flowchart TD` \
containing two subgraphs, `Before` and `After`, showing how control or data \
flows through the changed symbols. Only use symbols from the list.
3. `## Review order` — a short ordered list naming the files to read first and \
what to check in each.
4. `## Risks` — bullets for anything the structure suggests is risky, or \
`- None apparent` when nothing stands out.

Keep mermaid node labels short, quote every label, and never include code from \
the change.";

fn file_directory(file: &FileOverview) -> &str {
    file.path
        .parent()
        .map(|parent| parent.as_unix_str())
        .filter(|parent| !parent.is_empty())
        .unwrap_or("(root)")
}

fn status_marker(status: CommitFileStatus) -> &'static str {
    match status {
        CommitFileStatus::Added => "added",
        CommitFileStatus::Modified => "modified",
        CommitFileStatus::Deleted => "deleted",
    }
}

/// Mermaid labels are quoted, so quotes and newlines have to go.
fn escape(text: &str) -> String {
    text.replace('"', "'").replace(['\n', '\r'], " ")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(limit.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// Streams a model's narration of an overview, if the user has one configured.
pub(crate) fn narrate_overview(overview: &ChangeOverview, cx: &mut App) -> Task<Result<String>> {
    use futures::StreamExt as _;
    use language_model::{
        LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage, Role,
    };

    let registry = LanguageModelRegistry::read_global(cx);
    let Some(model) = registry.default_model() else {
        return Task::ready(Err(anyhow::anyhow!(
            "Configure a language model in the agent panel to narrate changes"
        )));
    };
    let provider = match registry.provider_for_model(&model) {
        Ok(provider) => provider,
        Err(error) => return Task::ready(Err(anyhow::anyhow!(error))),
    };
    let content = format!("{OVERVIEW_INSTRUCTIONS}\n\n{}", overview.to_prompt());

    cx.spawn(async move |cx: &mut AsyncApp| {
        if let Some(authenticate) =
            cx.update(|cx| (!provider.is_authenticated(cx)).then(|| provider.authenticate(cx)))
        {
            authenticate.await?;
        }
        let request = LanguageModelRequest {
            thread_id: None,
            prompt_cache_key: None,
            prompt_id: None,
            intent: None,
            messages: vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![content.into()],
                cache: false,
                reasoning_details: None,
            }],
            tools: Vec::new(),
            tool_choice: None,
            stop: Vec::new(),
            temperature: None,
            thinking_allowed: false,
            thinking_effort: None,
            speed: None,
            compact_at_tokens: None,
            max_output_tokens: None,
        };
        let mut response = provider.stream_completion_text(&model, request, cx).await?;
        let mut narration = String::new();
        while let Some(chunk) = response.stream.next().await {
            narration.push_str(&chunk?);
        }
        anyhow::ensure!(
            !narration.trim().is_empty(),
            "The model returned an empty overview"
        );
        Ok(narration)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::rel_path::rel_path;

    fn overview() -> ChangeOverview {
        ChangeOverview {
            title: "example/project #7".into(),
            files: vec![
                FileOverview {
                    path: RepoPath::from_rel_path(rel_path("crates/git_ui/src/git_panel.rs")),
                    status: CommitFileStatus::Modified,
                    stat: DiffStat {
                        added: 10,
                        deleted: 2,
                    },
                    is_binary: false,
                    symbols: vec![
                        ChangedSymbol {
                            name: "fn toggle".into(),
                            change: SymbolChange::Added,
                        },
                        ChangedSymbol {
                            name: "fn render".into(),
                            change: SymbolChange::Modified,
                        },
                    ],
                },
                FileOverview {
                    path: RepoPath::from_rel_path(rel_path("README.md")),
                    status: CommitFileStatus::Added,
                    stat: DiffStat {
                        added: 3,
                        deleted: 0,
                    },
                    is_binary: false,
                    symbols: Vec::new(),
                },
            ],
            commits: vec![("abc1234".into(), "Add toggle".into())],
            call_edges: vec![CallEdge {
                caller: "fn caller".into(),
                caller_path: Some("crates/git_ui/src/git_ui.rs".into()),
                callee: "fn toggle".into(),
            }],
        }
    }

    #[test]
    fn test_change_overview_markdown() {
        let markdown = overview().to_markdown();
        assert!(markdown.contains("2 file(s) changed, +13 −2"));
        assert!(markdown.contains("```mermaid\nflowchart LR"));
        assert!(markdown.contains("```mermaid\ngitGraph"));
        assert!(markdown.contains("+ fn toggle"));
        assert!(markdown.contains("~ fn render"));
        assert!(markdown.contains("| `README.md` | added +3 −0 | — |"));
    }

    #[test]
    fn test_change_overview_prompt_lists_structure_only() {
        let prompt = overview().to_prompt();
        assert!(prompt.contains("Files changed: 2, +13 −2"));
        assert!(prompt.contains("- abc1234 Add toggle"));
        assert!(prompt.contains("    + fn toggle"));
        assert!(prompt.contains("- fn caller (crates/git_ui/src/git_ui.rs) calls fn toggle"));
        assert!(!prompt.contains("```"), "the prompt must not contain code");
    }

    #[test]
    fn test_changed_symbols() {
        let old_symbols = vec![
            ("fn kept".into(), "body".to_string()),
            ("fn changed".into(), "old".to_string()),
            ("fn gone".into(), "body".to_string()),
        ];
        let new_symbols = vec![
            ("fn kept".into(), "body".to_string()),
            ("fn changed".into(), "new".to_string()),
            ("fn fresh".into(), "body".to_string()),
        ];
        assert_eq!(
            changed_symbols(&old_symbols, &new_symbols),
            vec![
                ChangedSymbol {
                    name: "fn changed".into(),
                    change: SymbolChange::Modified
                },
                ChangedSymbol {
                    name: "fn fresh".into(),
                    change: SymbolChange::Added
                },
                ChangedSymbol {
                    name: "fn gone".into(),
                    change: SymbolChange::Removed
                },
            ]
        );
    }

    #[test]
    fn test_mermaid_labels_are_escaped() {
        assert_eq!(escape("a \"quoted\"\nlabel"), "a 'quoted' label");
        assert_eq!(truncate("abcdef", 3), "ab…");
        assert_eq!(truncate("ab", 3), "ab");
    }
}
