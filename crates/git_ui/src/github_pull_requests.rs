use anyhow::{Context as _, Result, ensure};
use git::{GitHostingProviderRegistry, Oid, RemoteUrl, parse_git_remote_url};
use serde::Deserialize;
use std::{str::FromStr as _, sync::Arc};
use util::command::{Stdio, new_command};

pub(super) const RESULT_LIMIT: usize = 100;
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
    let output = new_command("gh")
        .args(arguments)
        // An explicit repository and a neutral cwd prevent local repository configuration from choosing the target.
        .current_dir(std::env::temp_dir())
        .env("GH_HOST", &repository.host)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("Could not run GitHub CLI (gh). Install gh and authenticate with gh auth login --hostname {} outside Zed, then refresh", repository.host))?;
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
