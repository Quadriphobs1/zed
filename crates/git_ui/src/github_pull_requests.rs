use anyhow::{Context as _, Result, ensure};
use git::{GitHostingProviderRegistry, Oid, RemoteUrl, parse_git_remote_url};
use serde::Deserialize;
use std::{str::FromStr as _, sync::Arc};
use util::command::{Stdio, new_command};

pub(super) const RESULT_LIMIT: usize = 100;
const GH_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const GH_STDERR_LIMIT: usize = 64 * 1024;
const GH_ERROR_LIMIT: usize = 4096;
const LIST_FIELDS: &str = "number,title,author,isDraft";
const DETAIL_FIELDS: &str =
    "number,title,author,isDraft,body,state,baseRefName,headRefName,baseRefOid,headRefOid";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GitHubRepository {
    host: String,
    owner: String,
    name: String,
}

impl GitHubRepository {
    pub(super) fn from_remote_url(
        registry: Arc<GitHostingProviderRegistry>,
        url: &str,
    ) -> Option<Self> {
        let (provider, remote) = parse_git_remote_url(registry, url)?;
        if !provider.supports_github_pull_requests() {
            return None;
        }
        let base_url = provider.base_url();
        if base_url.scheme() != "https"
            || base_url.path() != "/"
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.port().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return None;
        }
        let parsed_url = RemoteUrl::from_str(url).ok()?;
        if parsed_url.query().is_some()
            || parsed_url.fragment().is_some()
            || parsed_url.path().trim_matches('/').split('/').count() != 2
        {
            return None;
        }
        let owner = remote.owner.as_ref();
        let name = remote.repo.as_ref();
        let valid = |component: &str| {
            !component.is_empty()
                && !component.starts_with('-')
                && !matches!(component, "." | "..")
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        };
        if !valid(owner) || owner.starts_with('.') || !valid(name) {
            return None;
        }
        Some(Self {
            host: base_url.host_str()?.into(),
            owner: owner.into(),
            name: name.into(),
        })
    }

    pub(super) fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub(super) fn target(&self) -> String {
        format!("{}/{}", self.host, self.slug())
    }

    pub(super) fn pull_request_url(&self, number: u64) -> String {
        format!("https://{}/pull/{number}", self.target())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum PullRequestFilter {
    #[default]
    All,
    Assigned,
    ReviewRequested,
    Created,
}

impl PullRequestFilter {
    pub(super) const ALL: [Self; 4] = [
        Self::All,
        Self::Assigned,
        Self::ReviewRequested,
        Self::Created,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Assigned => "Assigned",
            Self::ReviewRequested => "Review requested",
            Self::Created => "Created by me",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PullRequestSummary {
    pub(super) number: u64,
    pub(super) title: String,
    author: Option<Author>,
    pub(super) is_draft: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Author {
    login: String,
}

impl PullRequestSummary {
    pub(super) fn author_login(&self) -> &str {
        self.author.as_ref().map_or("ghost", |author| &author.login)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PullRequestDetail {
    #[serde(flatten)]
    pub(super) summary: PullRequestSummary,
    pub(super) body: String,
    pub(super) state: String,
    pub(super) base_ref_name: String,
    pub(super) head_ref_name: String,
    pub(super) base_ref_oid: String,
    pub(super) head_ref_oid: String,
}

pub(super) const ACTIVITY_PAGE_SIZE: usize = 20;
pub(super) const ACTIVITY_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivitySection {
    Timeline,
    Commits,
    Threads,
}

#[derive(Clone, Debug)]
pub(super) struct ActivityComment {
    pub(super) author: String,
    pub(super) date: String,
    pub(super) body: String,
    pub(super) hunk: String,
}

#[derive(Clone, Debug)]
pub(super) enum ActivityKind {
    Commit {
        oid: Oid,
        title: String,
    },
    Comment,
    Review {
        state: String,
    },
    Thread {
        path: String,
        line: Option<u64>,
        resolved: bool,
        outdated: bool,
        comments: Vec<ActivityComment>,
        more_comments: bool,
    },
    Event {
        name: String,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ActivityItem {
    pub(super) id: String,
    pub(super) author: String,
    pub(super) date: String,
    pub(super) body: String,
    pub(super) kind: ActivityKind,
}

#[derive(Clone, Debug)]
pub(super) struct ActivityPage {
    pub(super) head: String,
    pub(super) items: Vec<ActivityItem>,
    pub(super) cursor: Option<String>,
}

/// Uses gh's existing authentication without logging in, fetching Git objects, or writing to GitHub.
pub(super) struct GitHubPullRequestService;

impl GitHubPullRequestService {
    pub(super) async fn activity(
        repository: &GitHubRepository,
        number: u64,
        section: ActivitySection,
        cursor: Option<String>,
    ) -> Result<ActivityPage> {
        let arguments = activity_arguments(repository, number, section, cursor);
        parse_activity(&run_gh(repository, arguments).await?, number, section)
    }

    pub(super) async fn list(
        repository: &GitHubRepository,
        filter: PullRequestFilter,
    ) -> Result<Vec<PullRequestSummary>> {
        parse_list(&run_gh(repository, list_arguments(repository, filter)).await?)
    }

    pub(super) async fn detail(
        repository: &GitHubRepository,
        number: u64,
    ) -> Result<PullRequestDetail> {
        parse_detail(
            &run_gh(repository, detail_arguments(repository, number)).await?,
            number,
        )
    }
}

fn list_arguments(repository: &GitHubRepository, filter: PullRequestFilter) -> Vec<String> {
    let mut arguments: Vec<String> = [
        "pr",
        "list",
        "--repo",
        &repository.target(),
        "--state",
        "open",
        "--limit",
        &RESULT_LIMIT.to_string(),
        "--json",
        LIST_FIELDS,
    ]
    .into_iter()
    .map(String::from)
    .collect();
    arguments.extend(
        match filter {
            PullRequestFilter::All => vec![],
            PullRequestFilter::Assigned => vec!["--assignee", "@me"],
            PullRequestFilter::ReviewRequested => vec!["--search", "review-requested:@me"],
            PullRequestFilter::Created => vec!["--author", "@me"],
        }
        .into_iter()
        .map(String::from),
    );
    arguments
}

fn detail_arguments(repository: &GitHubRepository, number: u64) -> Vec<String> {
    [
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &repository.target(),
        "--json",
        DETAIL_FIELDS,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

async fn run_gh(repository: &GitHubRepository, arguments: Vec<String>) -> Result<Vec<u8>> {
    let mut child = new_command("gh")
        .args(arguments)
        // An explicit repository and a neutral cwd prevent local repository configuration from choosing the target.
        .current_dir(std::env::temp_dir())
        .env("GH_HOST", &repository.host)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Could not run GitHub CLI (gh). Install gh and authenticate with gh auth login --hostname {} outside Zed, then refresh", repository.host))?;
    let stdout = child.stdout.take().context("Missing GitHub CLI output")?;
    let stderr = child.stderr.take().context("Missing GitHub CLI errors")?;
    // Stop oversized responses before parsing or retaining nested review content.
    let (stdout, stderr, status) = futures::try_join!(
        read_gh_output(stdout, GH_RESPONSE_LIMIT),
        read_gh_output(stderr, GH_STDERR_LIMIT),
        async { anyhow::Ok(child.status().await?) },
    )?;
    ensure!(
        status.success(),
        "GitHub CLI failed ({}): {}. Check gh auth status, repository access, and GitHub rate limits, then refresh. Zed does not start interactive login",
        status,
        String::from_utf8_lossy(&stderr)
            .chars()
            .take(GH_ERROR_LIMIT)
            .collect::<String>()
    );
    Ok(stdout)
}

async fn read_gh_output(
    reader: impl futures::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>> {
    use futures::AsyncReadExt as _;
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= limit,
        "GitHub response exceeded the size limit; open GitHub for this activity"
    );
    Ok(bytes)
}

fn parse_list(bytes: &[u8]) -> Result<Vec<PullRequestSummary>> {
    let entries: Vec<PullRequestSummary> =
        serde_json::from_slice(bytes).context("Invalid GitHub PR list response")?;
    ensure!(
        entries.len() <= RESULT_LIMIT,
        "GitHub PR list exceeded the result limit"
    );
    ensure!(
        entries.iter().all(|entry| entry.number > 0),
        "Invalid GitHub PR number"
    );
    Ok(entries)
}

fn parse_detail(bytes: &[u8], number: u64) -> Result<PullRequestDetail> {
    let detail: PullRequestDetail =
        serde_json::from_slice(bytes).context("Invalid GitHub PR detail response")?;
    ensure!(
        number > 0 && detail.summary.number == number,
        "GitHub returned a different pull request"
    );
    for sha in [&detail.base_ref_oid, &detail.head_ref_oid] {
        ensure!(
            sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "GitHub returned an invalid commit SHA"
        );
        Oid::from_str(sha).context("Invalid GitHub commit SHA")?;
    }
    Ok(detail)
}

fn activity_arguments(
    repository: &GitHubRepository,
    number: u64,
    section: ActivitySection,
    cursor: Option<String>,
) -> Vec<String> {
    let query = match section {
        ActivitySection::Timeline => {
            r#"query($owner:String!,$name:String!,$number:Int!,$limit:Int!,$cursor:String) {
            repository(owner:$owner,name:$name) { pullRequest(number:$number) {
                number headRefOid timelineItems(first:$limit,after:$cursor) {
                    pageInfo { hasNextPage endCursor } edges { cursor node { __typename
                        ... on PullRequestCommit { commit { oid messageHeadline messageBody committedDate author { name user { login } } } }
                        ... on IssueComment { body createdAt author { login } }
                        ... on PullRequestReview { body submittedAt state author { login } }
                    } }
                }
            } }
        }"#
        }
        ActivitySection::Commits => {
            r#"query($owner:String!,$name:String!,$number:Int!,$limit:Int!,$cursor:String) {
            repository(owner:$owner,name:$name) { pullRequest(number:$number) {
                number headRefOid commits(first:$limit,after:$cursor) {
                    pageInfo { hasNextPage endCursor } edges { cursor node {
                        commit { oid messageHeadline messageBody committedDate author { name user { login } } }
                    } }
                }
            } }
        }"#
        }
        ActivitySection::Threads => {
            r#"query($owner:String!,$name:String!,$number:Int!,$limit:Int!,$cursor:String) {
            repository(owner:$owner,name:$name) { pullRequest(number:$number) {
                number headRefOid reviewThreads(first:$limit,after:$cursor) {
                    pageInfo { hasNextPage endCursor } edges { cursor node {
                        path line isResolved isOutdated comments(first:$limit) {
                            pageInfo { hasNextPage } nodes { body createdAt author { login } diffHunk }
                        }
                    } }
                }
            } }
        }"#
        }
    };
    let mut arguments = vec![
        "api".into(),
        "graphql".into(),
        "--hostname".into(),
        repository.host.clone(),
        "--method".into(),
        "POST".into(),
        "-f".into(),
        format!("query={query}"),
        "-f".into(),
        format!("owner={}", repository.owner),
        "-f".into(),
        format!("name={}", repository.name),
        "-F".into(),
        format!("number={number}"),
        "-F".into(),
        format!("limit={ACTIVITY_PAGE_SIZE}"),
    ];
    if let Some(cursor) = cursor {
        arguments.extend(["-f".into(), format!("cursor={cursor}")]);
    }
    arguments
}

fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing GitHub activity {key}"))
}

fn activity_oid(value: &str) -> Result<Oid> {
    ensure!(
        value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid GitHub activity commit ID"
    );
    Oid::from_str(value).context("Invalid GitHub activity commit ID")
}

fn activity_actor(value: &serde_json::Value) -> String {
    value["author"]["login"]
        .as_str()
        .unwrap_or("ghost")
        .to_owned()
}

fn parse_activity(bytes: &[u8], number: u64, section: ActivitySection) -> Result<ActivityPage> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("Invalid GitHub activity response")?;
    // GraphQL can return HTTP success and partial data alongside authorization or schema errors.
    ensure!(
        value
            .get("errors")
            .is_none_or(|errors| errors.as_array().is_some_and(Vec::is_empty)),
        "GitHub returned activity errors; check access/rate limits and retry, or open GitHub"
    );
    let pr = &value["data"]["repository"]["pullRequest"];
    ensure!(
        number > 0 && pr["number"].as_u64() == Some(number),
        "GitHub returned a different pull request"
    );
    let head = activity_oid(json_string(pr, "headRefOid")?)?.to_string();
    let connection = &pr[match section {
        ActivitySection::Timeline => "timelineItems",
        ActivitySection::Commits => "commits",
        ActivitySection::Threads => "reviewThreads",
    }];
    let edges = connection["edges"]
        .as_array()
        .context("Missing GitHub activity page")?;
    ensure!(
        edges.len() <= ACTIVITY_PAGE_SIZE,
        "GitHub activity page exceeded the limit"
    );
    let has_more = connection["pageInfo"]["hasNextPage"]
        .as_bool()
        .context("Missing GitHub activity pagination")?;
    let cursor = if has_more {
        let cursor = json_string(&connection["pageInfo"], "endCursor")?;
        ensure!(
            !cursor.is_empty() && !edges.is_empty(),
            "Invalid GitHub activity cursor"
        );
        Some(cursor.to_owned())
    } else {
        None
    };
    let mut items = Vec::with_capacity(edges.len());
    for edge in edges {
        let id = json_string(edge, "cursor")?.to_owned();
        ensure!(
            !id.is_empty() && !items.iter().any(|item: &ActivityItem| item.id == id),
            "Duplicate or missing GitHub activity cursor"
        );
        let node = &edge["node"];
        let mut item = ActivityItem {
            id,
            author: String::new(),
            date: String::new(),
            body: String::new(),
            kind: ActivityKind::Event {
                name: "Unavailable event".into(),
            },
        };
        if node.is_null() {
            items.push(item);
            continue;
        }
        if section == ActivitySection::Threads {
            let comments = node["comments"]["nodes"]
                .as_array()
                .context("Missing GitHub thread comments")?;
            ensure!(
                comments.len() <= ACTIVITY_PAGE_SIZE,
                "GitHub review thread exceeded the comment limit"
            );
            let comments = comments
                .iter()
                .map(|comment| {
                    Ok(ActivityComment {
                        author: activity_actor(comment),
                        date: json_string(comment, "createdAt")?.into(),
                        body: json_string(comment, "body")?.into(),
                        hunk: json_string(comment, "diffHunk")?.into(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let line = if node["line"].is_null() {
                None
            } else {
                Some(
                    node["line"]
                        .as_u64()
                        .context("Invalid GitHub review line")?,
                )
            };
            item.kind = ActivityKind::Thread {
                path: json_string(node, "path")?.into(),
                line,
                resolved: node["isResolved"]
                    .as_bool()
                    .context("Missing thread resolution")?,
                outdated: node["isOutdated"]
                    .as_bool()
                    .context("Missing thread status")?,
                comments,
                more_comments: node["comments"]["pageInfo"]["hasNextPage"]
                    .as_bool()
                    .context("Missing thread pagination")?,
            };
            items.push(item);
            continue;
        }
        if section == ActivitySection::Commits {
            let commit = &node["commit"];
            item.author = commit["author"]["user"]["login"]
                .as_str()
                .or_else(|| commit["author"]["name"].as_str())
                .unwrap_or("ghost")
                .into();
            item.date = json_string(commit, "committedDate")?.into();
            item.body = json_string(commit, "messageBody")?.into();
            item.kind = ActivityKind::Commit {
                oid: activity_oid(json_string(commit, "oid")?)?,
                title: json_string(commit, "messageHeadline")?.into(),
            };
            items.push(item);
            continue;
        }
        item.kind = match json_string(node, "__typename")? {
            "PullRequestCommit" => {
                let commit = &node["commit"];
                item.author = commit["author"]["user"]["login"]
                    .as_str()
                    .or_else(|| commit["author"]["name"].as_str())
                    .unwrap_or("ghost")
                    .into();
                item.date = json_string(commit, "committedDate")?.into();
                item.body = json_string(commit, "messageBody")?.into();
                ActivityKind::Commit {
                    oid: activity_oid(json_string(commit, "oid")?)?,
                    title: json_string(commit, "messageHeadline")?.into(),
                }
            }
            "IssueComment" => {
                item.author = activity_actor(node);
                item.date = json_string(node, "createdAt")?.into();
                item.body = json_string(node, "body")?.into();
                ActivityKind::Comment
            }
            "PullRequestReview" => {
                item.author = activity_actor(node);
                item.date = node["submittedAt"].as_str().unwrap_or_default().into();
                item.body = json_string(node, "body")?.into();
                ActivityKind::Review {
                    state: json_string(node, "state")?.into(),
                }
            }
            name => ActivityKind::Event { name: name.into() },
        };
        items.push(item);
    }
    Ok(ActivityPage {
        head,
        items,
        cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git_hosting_providers::{Github, Gitlab};

    fn parse_remote(url: &str) -> Option<GitHubRepository> {
        let registry = Arc::new(GitHostingProviderRegistry::new());
        registry.register_hosting_provider(Arc::new(Github::public_instance()));
        registry.register_hosting_provider(Arc::new(Github::new(
            "Company code",
            "https://github.enterprise.test".parse().expect("base URL"),
        )));
        registry.register_hosting_provider(Arc::new(Github::new(
            "Configured custom host",
            "https://code.example.test".parse().expect("base URL"),
        )));
        registry.register_hosting_provider(Arc::new(Gitlab::public_instance()));
        GitHubRepository::from_remote_url(registry, url)
    }

    #[test]
    fn test_enterprise_pr_target() {
        let repository = parse_remote("git@github.enterprise.test:team/project.git")
            .expect("Enterprise GitHub remote");
        assert_eq!(
            repository.pull_request_url(42),
            "https://github.enterprise.test/team/project/pull/42"
        );
        assert_eq!(
            list_arguments(&repository, PullRequestFilter::All)[3],
            "github.enterprise.test/team/project"
        );
        assert_eq!(
            detail_arguments(&repository, 42)[4],
            "github.enterprise.test/team/project"
        );
    }

    #[test]
    fn test_github_pull_requests_remote_parsing() {
        for url in [
            "https://github.com/zed-industries/zed.git",
            "git@github.com:zed-industries/zed.git",
            "ssh://git@github.com/zed-industries/zed",
            "https://github.com/zed-industries/zed/",
        ] {
            let repository = parse_remote(url).expect("GitHub remote");
            assert_eq!(repository.slug(), "zed-industries/zed");
        }
        assert_eq!(
            parse_remote("https://github.com/owner/.github.git")
                .expect("dot-prefixed repository")
                .slug(),
            "owner/.github"
        );
        for url in [
            "https://github.example.com/owner/repo",
            "https://github.com.evil.test/owner/repo",
            "https://github.com/owner/repo?query",
            "https://github.com/owner/repo/extra",
            "https://github.com/../repo",
            "https://github.com/owner/..",
            "https://github.com/owner/.",
            "https://github.com/-owner/repo",
            "file:///repo",
            "git@other:owner/repo",
        ] {
            assert!(parse_remote(url).is_none(), "{url}");
        }
    }

    #[test]
    fn test_configured_pr_provider() {
        for url in [
            "https://user@code.example.test/team/project.git",
            "git@code.example.test:team/project.git",
            "ssh://git@code.example.test:2222/team/project.git",
        ] {
            let repository = parse_remote(url).expect("configured GitHub provider");
            assert_eq!(repository.target(), "code.example.test/team/project");
            assert_eq!(
                repository.pull_request_url(1),
                "https://code.example.test/team/project/pull/1"
            );
        }
        assert!(parse_remote("git@gitlab.com:team/project.git").is_none());
        assert!(parse_remote("git@unconfigured.example.test:team/project.git").is_none());
    }

    #[test]
    fn test_github_pull_requests_arguments_are_bounded_and_explicit() {
        let repository = parse_remote("git@github.com:owner/repo.git").expect("GitHub remote");
        for (filter, expected) in [
            (PullRequestFilter::All, vec![]),
            (PullRequestFilter::Assigned, vec!["--assignee", "@me"]),
            (
                PullRequestFilter::ReviewRequested,
                vec!["--search", "review-requested:@me"],
            ),
            (PullRequestFilter::Created, vec!["--author", "@me"]),
        ] {
            let arguments = list_arguments(&repository, filter);
            assert_eq!(
                &arguments[..9],
                &[
                    "pr",
                    "list",
                    "--repo",
                    "github.com/owner/repo",
                    "--state",
                    "open",
                    "--limit",
                    "100",
                    "--json"
                ]
            );
            assert_eq!(arguments.get(9).map(String::as_str), Some(LIST_FIELDS));
            assert_eq!(&arguments[10..], expected);
        }
        assert_eq!(
            detail_arguments(&repository, 42),
            [
                "pr",
                "view",
                "42",
                "--repo",
                "github.com/owner/repo",
                "--json",
                DETAIL_FIELDS
            ]
        );
        assert_eq!(
            repository.pull_request_url(42),
            "https://github.com/owner/repo/pull/42"
        );
    }

    fn activity_response(section: ActivitySection, nodes: serde_json::Value) -> serde_json::Value {
        let connection = match section {
            ActivitySection::Timeline => "timelineItems",
            ActivitySection::Commits => "commits",
            ActivitySection::Threads => "reviewThreads",
        };
        let mut pr = serde_json::json!({"number":42,"headRefOid":"1111111111111111111111111111111111111111"});
        pr[connection] = serde_json::json!({"edges": nodes, "pageInfo":{"hasNextPage":true,"endCursor":"next-page"}});
        serde_json::json!({"data":{"repository":{"pullRequest":pr}}})
    }

    #[test]
    fn test_pr_activity_output() {
        futures::executor::block_on(async {
            assert_eq!(
                read_gh_output(futures::io::Cursor::new(b"body"), 4)
                    .await
                    .expect("bounded output"),
                b"body"
            );
            assert!(
                read_gh_output(futures::io::Cursor::new(b"large"), 4)
                    .await
                    .is_err()
            );
        });
        let repository =
            parse_remote("git@github.enterprise.test:team/project.git").expect("target");
        for section in [
            ActivitySection::Timeline,
            ActivitySection::Commits,
            ActivitySection::Threads,
        ] {
            let arguments =
                activity_arguments(&repository, 42, section, Some("opaque{cursor}".into()));
            assert_eq!(
                &arguments[..6],
                [
                    "api",
                    "graphql",
                    "--hostname",
                    "github.enterprise.test",
                    "--method",
                    "POST"
                ]
            );
            assert!(arguments.iter().any(|arg| arg == "number=42"));
            assert!(arguments.iter().any(|arg| arg == "limit=20"));
            assert!(arguments.iter().any(|arg| arg == "cursor=opaque{cursor}"));
            assert!(!arguments.iter().any(|arg| arg == "--paginate"));
            assert!(arguments.iter().any(|arg| arg.starts_with("query=query(")));
        }
    }

    #[test]
    fn test_pr_commits_section() {
        let oid = "a".repeat(40);
        let value = activity_response(
            ActivitySection::Commits,
            serde_json::json!([{
                "cursor": "commit-1",
                "node": { "commit": {
                    "oid": oid,
                    "messageHeadline": "Fix the thing",
                    "messageBody": "Details",
                    "committedDate": "2024-04-01T00:00:00Z",
                    "author": { "name": "Ada", "user": { "login": "ada" } }
                } }
            }]),
        );
        let page = parse_activity(value.to_string().as_bytes(), 42, ActivitySection::Commits)
            .expect("commits page");
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        assert_eq!(item.author, "ada");
        assert_eq!(item.body, "Details");
        match &item.kind {
            ActivityKind::Commit { oid: parsed, title } => {
                assert_eq!(parsed.to_string(), oid);
                assert_eq!(title, "Fix the thing");
            }
            kind => panic!("the commits section must only yield commits, got {kind:?}"),
        }
    }

    #[test]
    fn test_pr_activity_pages() {
        let value = activity_response(
            ActivitySection::Timeline,
            serde_json::json!([
                {"cursor":"commit-cursor","node":{"__typename":"PullRequestCommit","commit":{
                    "oid":"2222222222222222222222222222222222222222","messageHeadline":"Change","messageBody":"Details",
                    "committedDate":"2026-01-01T00:00:00Z","author":{"name":"Contributor","user":null}}}},
                {"cursor":"comment-cursor","node":{"__typename":"IssueComment","id":"comment-id","author":null,"createdAt":"2026-01-02T00:00:00Z","body":"**Comment**"}},
                {"cursor":"review-cursor","node":{"__typename":"PullRequestReview","id":"review-id","author":{"login":"reviewer"},"submittedAt":null,"state":"APPROVED","body":"Looks fine"}},
                {"cursor":"event-cursor","node":{"__typename":"MergedEvent"}}
            ]),
        );
        let bytes = serde_json::to_vec(&value).expect("JSON");
        let page = parse_activity(&bytes, 42, ActivitySection::Timeline).expect("activity page");
        assert_eq!(page.cursor.as_deref(), Some("next-page"));
        assert_eq!(page.items.len(), 4);
        assert!(
            matches!(&page.items[0].kind, ActivityKind::Commit{oid,..} if oid.to_string() == "2222222222222222222222222222222222222222")
        );
        assert_eq!(page.items[0].author, "Contributor");
        assert_eq!(page.items[1].author, "ghost");
        assert_eq!(page.items[1].body, "**Comment**");
        assert!(matches!(&page.items[2].kind, ActivityKind::Review{state} if state == "APPROVED"));
        assert!(matches!(&page.items[3].kind, ActivityKind::Event{name} if name == "MergedEvent"));
        assert!(parse_activity(&bytes, 43, ActivitySection::Timeline).is_err());
        let mut invalid = value.clone();
        invalid["errors"] = serde_json::json!([{"message":"partial response"}]);
        assert!(
            parse_activity(
                &serde_json::to_vec(&invalid).expect("JSON"),
                42,
                ActivitySection::Timeline
            )
            .is_err()
        );
        invalid = value.clone();
        invalid["data"]["repository"]["pullRequest"]["timelineItems"]["pageInfo"]["endCursor"] =
            serde_json::Value::Null;
        assert!(
            parse_activity(
                &serde_json::to_vec(&invalid).expect("JSON"),
                42,
                ActivitySection::Timeline
            )
            .is_err()
        );
        invalid = value;
        invalid["data"]["repository"]["pullRequest"]["timelineItems"]["edges"][0]["node"]["commit"]
            ["oid"] = "main".into();
        assert!(
            parse_activity(
                &serde_json::to_vec(&invalid).expect("JSON"),
                42,
                ActivitySection::Timeline
            )
            .is_err()
        );
    }

    #[test]
    fn test_pr_activity_threads() {
        let value = activity_response(
            ActivitySection::Threads,
            serde_json::json!([
                {"cursor":"thread-cursor","node":{"id":"thread-id","path":"src/example.rs","line":12,"isResolved":true,"isOutdated":true,
                    "comments":{"nodes":[{"author":{"login":"reviewer"},"createdAt":"2026-01-01T00:00:00Z","body":"Comment","diffHunk":"@@ -1 +1 @@\n-old\n+new"}],"pageInfo":{"hasNextPage":true}}}}
            ]),
        );
        let page = parse_activity(
            &serde_json::to_vec(&value).expect("JSON"),
            42,
            ActivitySection::Threads,
        )
        .expect("threads page");
        assert!(
            matches!(&page.items[0].kind, ActivityKind::Thread{path,line:Some(12),resolved:true,outdated:true,comments,more_comments:true} if path == "src/example.rs" && comments.len()==1 && comments[0].body=="Comment")
        );
        let oversized = activity_response(
            ActivitySection::Timeline,
            serde_json::json!(vec![
                serde_json::json!({"cursor":"x","node":{"__typename":"MergedEvent"}});
                ACTIVITY_PAGE_SIZE + 1
            ]),
        );
        assert!(
            parse_activity(
                &serde_json::to_vec(&oversized).expect("JSON"),
                42,
                ActivitySection::Timeline
            )
            .is_err()
        );
    }

    #[test]
    fn test_github_pull_requests_json_and_pinned_metadata() {
        let list = parse_list(br#"[{"number":42,"title":"A PR","author":null,"isDraft":true}]"#)
            .expect("list");
        assert_eq!(list[0].author_login(), "ghost");
        assert!(list[0].is_draft);
        assert!(parse_list(b"not json").is_err());
        let too_many = vec![
            serde_json::json!({"number": 1, "title": "PR", "isDraft": false});
            RESULT_LIMIT + 1
        ];
        assert!(parse_list(&serde_json::to_vec(&too_many).expect("json")).is_err());
        assert!(parse_list(br#"[{"number":0,"title":"bad","isDraft":false}]"#).is_err());
        let value = serde_json::json!({
            "number": 42, "title": "Title", "author": {"login": "octocat"},
            "isDraft": false, "body": "# Description", "state": "OPEN",
            "baseRefName": "main", "headRefName": "feature",
            "baseRefOid": "1111111111111111111111111111111111111111",
            "headRefOid": "2222222222222222222222222222222222222222"
        });
        let bytes = serde_json::to_vec(&value).expect("json");
        let detail = parse_detail(&bytes, 42).expect("detail");
        assert_eq!(detail.summary.author_login(), "octocat");
        assert_eq!(detail.body, "# Description");
        assert!(parse_detail(&bytes, 43).is_err());
        let mut invalid = value;
        invalid["headRefOid"] = "main".into();
        assert!(parse_detail(&serde_json::to_vec(&invalid).expect("json"), 42).is_err());
    }
}
