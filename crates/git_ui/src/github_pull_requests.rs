use anyhow::{Context as _, Result, ensure};
use git::Oid;
use serde::Deserialize;
use std::str::FromStr as _;
use util::command::{Stdio, new_command};

pub(super) const RESULT_LIMIT: usize = 100;
const LIST_FIELDS: &str = "number,title,author,isDraft";
const DETAIL_FIELDS: &str =
    "number,title,author,isDraft,body,state,baseRefName,headRefName,baseRefOid,headRefOid";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GitHubRepository {
    owner: String,
    name: String,
}

impl GitHubRepository {
    pub(super) fn from_remote_url(url: &str) -> Option<Self> {
        let path = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("git@github.com:"))
            .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
        let path = path.strip_suffix('/').unwrap_or(path);
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
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
            owner: owner.into(),
            name: name.into(),
        })
    }

    pub(super) fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub(super) fn pull_request_url(&self, number: u64) -> String {
        format!("https://github.com/{}/pull/{number}", self.slug())
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

#[derive(Debug, Deserialize)]
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

/// Uses gh's existing authentication without logging in, fetching Git objects, or writing to GitHub.
pub(super) struct GitHubPullRequestService;

impl GitHubPullRequestService {
    pub(super) async fn list(
        repository: &GitHubRepository,
        filter: PullRequestFilter,
    ) -> Result<Vec<PullRequestSummary>> {
        parse_list(&run_gh(list_arguments(repository, filter)).await?)
    }

    pub(super) async fn detail(
        repository: &GitHubRepository,
        number: u64,
    ) -> Result<PullRequestDetail> {
        parse_detail(&run_gh(detail_arguments(repository, number)).await?, number)
    }
}

fn list_arguments(repository: &GitHubRepository, filter: PullRequestFilter) -> Vec<String> {
    let mut arguments: Vec<String> = [
        "pr",
        "list",
        "--repo",
        &format!("github.com/{}", repository.slug()),
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
        &format!("github.com/{}", repository.slug()),
        "--json",
        DETAIL_FIELDS,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

async fn run_gh(arguments: Vec<String>) -> Result<Vec<u8>> {
    let output = new_command("gh")
        .args(arguments)
        // An explicit repository and a neutral cwd prevent local repository configuration from choosing the target.
        .current_dir(std::env::temp_dir())
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("Could not run GitHub CLI (gh). Install gh and authenticate with gh auth login outside Zed, then refresh")?;
    ensure!(
        output.status.success(),
        "GitHub CLI failed ({}): {}. Check gh auth status, repository access, and GitHub rate limits, then refresh. Zed does not start interactive login",
        output.status,
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(4096)
            .collect::<String>()
    );
    Ok(output.stdout)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_pull_requests_remote_parsing() {
        for url in [
            "https://github.com/zed-industries/zed.git",
            "git@github.com:zed-industries/zed.git",
            "ssh://git@github.com/zed-industries/zed",
            "https://github.com/zed-industries/zed/",
        ] {
            let repository = GitHubRepository::from_remote_url(url).expect("GitHub remote");
            assert_eq!(repository.slug(), "zed-industries/zed");
        }
        assert_eq!(
            GitHubRepository::from_remote_url("https://github.com/owner/.github.git")
                .expect("dot-prefixed repository")
                .slug(),
            "owner/.github"
        );
        for url in [
            "https://github.example.com/owner/repo",
            "https://github.com.evil.test/owner/repo",
            "https://token@github.com/owner/repo",
            "https://github.com/owner/repo?query",
            "https://github.com/owner/repo/extra",
            "https://github.com/../repo",
            "https://github.com/owner/..",
            "https://github.com/owner/.",
            "https://github.com/-owner/repo",
            "file:///repo",
            "git@other:owner/repo",
        ] {
            assert!(GitHubRepository::from_remote_url(url).is_none(), "{url}");
        }
    }

    #[test]
    fn test_github_pull_requests_arguments_are_bounded_and_explicit() {
        let repository = GitHubRepository::from_remote_url("git@github.com:owner/repo.git")
            .expect("GitHub remote");
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
