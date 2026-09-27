# Pull request browser prototype

This fork adds a read-only **Pull Requests** tab to the Git panel. Open it directly with **git panel: activate pull requests tab** in the command palette.

## Scope and prerequisites

- Open a **trusted local project** with a GitHub remote recognized by Zed's hosting-provider registry, including GitHub Enterprise. Remote URLs and host settings use the existing Git integration; SSH/remote **projects** and non-GitHub providers remain unsupported. SSH Git URLs and HTTPS URLs with usernames are supported.
- Install GitHub CLI (`gh`) on Zed's executable search path and authenticate outside Zed (`gh auth login --hostname HOST`, then `gh auth status --hostname HOST`). PR data still requires the host's API; local Git history and SSH credentials alone cannot supply it. Zed reuses the CLI session without initiating login.
- Select a GitHub remote explicitly. `origin` may be your fork; select `upstream` to browse the upstream repository. **Reload remotes** clears that choice and rereads repository configuration.
- Lists contain at most **100 open PRs**. This is a bounded prototype, not a complete/paginated listing. Use GitHub for additional results.
- **Open Diff on GitHub** opens the current published file changes in your browser. Native PR diffs are deferred until there is a safe pinned-object fetch API. This tab never substitutes a local branch/worktree diff for a published PR diff. The separate **Compare HEAD with Upstream (No Fetch)** feature only compares locally available committed revisions.

For Enterprise hosts that Zed does not detect automatically, use its existing `git_hosting_providers` setting, then **Reload remotes**:

```json
{
  "git_hosting_providers": [
    {
      "provider": "github",
      "name": "Company GitHub",
      "base_url": "https://code.example.com"
    }
  ]
}
```

The selected provider supplies both the API hostname and browser-link destination. Only HTTPS root hosting URLs on the default port are supported; a custom SSH Git port does not change the API port.

## Manual checks

1. Open the Git panel. Switch among Changes, History, and Pull Requests; ensure only the selected tab is highlighted. Reopen the panel and check focus. Changes/History keyboard operations must not operate on hidden files while Pull Requests is active.
2. In a clone with both a fork `origin` and upstream remote, select each in turn. Confirm repository names and PRs differ as expected. Verify no GitHub requests are made until a remote is selected. Switch active local repositories during a slow request; old results/errors must not appear in the new repository.
3. Exercise **All**, **Assigned**, **Review requested**, and **Created by me**. Compare to GitHub while signed into the same account as `gh`. Rapidly switch filters/remotes and refresh; old results/errors must not replace the current selection. Check loading, empty, and explicit 100-result-limit messages. Arrow keys select rows and Enter opens a description. Resize the panel: titles and authors should stay left-aligned and truncate, and clicking either line should open the PR. Rows use History's virtualized-list pattern.
4. Open a PR. Check number, author (including deleted users), draft/state, branch names, full base/head commit IDs, and rendered Markdown. Check **Open on GitHub** and **Open Diff on GitHub** targets. Refresh after an edit or force-push: metadata is a snapshot, whereas browser links always show current GitHub state. Closed/merged PRs opened from a stale list should show their current state in the description.
5. With `gh` absent from PATH, unauthenticated, denied repository access, offline, or rate limited, confirm an actionable visible error and a working retry after repair. No interactive login should open. Avoid logging credentials or changing your normal account merely to simulate failures.
6. In a restricted workspace, confirm no PR CLI requests occur and a trust message is visible. Trust the workspace through Zed's existing controls, then reload remotes. Try an SSH/remote project, an unsupported remote host, and a repository with no remotes; each should show an explicit unsupported/empty state rather than running a local command against a remote path.
7. Before and after browsing, compare `git status --porcelain=v1`, HEAD, refs, index bytes, staged/unstaged edits, and untracked files. Browsing must not fetch, checkout, post reviews, write GitHub data, or change the working tree/index.

## Build and review

```sh
git fetch origin
git switch --track origin/feature/pull-request-browser
cargo build -p zed
./target/debug/zed --user-data-dir /tmp/zed-pr-browser-review .
```

Requires the repository's pinned Rust toolchain and platform build prerequisites (see [macOS setup](macos.md)). On an existing local feature branch, use `git pull --ff-only` instead of creating it again. The separate user-data directory avoids reusing the normal Zed database; settings are still shared.

Run **git: compare head with upstream** to compare committed HEAD to its configured remote-tracking tip. No fetch occurs and staged/unstaged edits are excluded. Verify identical tips show an empty state, divergent tips show the direct tree comparison, and missing/local-only upstreams produce actionable errors. Loading, error, and empty states must retain keyboard focus. The comparison is read-only.

## Verification

Passed on macOS with Rust 1.98.1:

- `cargo test -p git_ui --lib`: 168 tests, including Enterprise target resolution and rendered row geometry/click coverage.
- `cargo test -p git_hosting_providers --lib`: 119 tests.
- `cargo test -p git test_load_revision_diff -- --nocapture`: 2 tests.
- `cargo test -p git test_load_commit -- --nocapture`: 3 regression tests.
- `cargo build -p zed`: debug application builds; `target/debug/zed --help` runs.
- Changed-source `rustfmt --check --edition 2024` and `git diff --check`.
- Live read-only `gh pr list` and `gh pr view` requests against zed-industries/zed returned the expected metadata fields and full commit IDs.

Two independent source reviews preceded commit. One identified missing placeholder focus; a rendered GPUI regression failed before the fix and passed afterward. The complete Git UI test suite then passed.

The Enterprise and list-layout follow-up also received two independent reviews. Both found no issues. Enterprise remote rejection and the metadata-line click regression were observed failing before their fixes and passing afterward. The row test uses fixture data without network requests; it checks geometry and selection, not opening a live PR.

Not yet verified: live Enterprise authentication, interactive application checks above, all account filters against live GitHub, other platforms, release builds, and linting (`./script/clippy`). Build warnings include a large debug unwind section and future incompatibility in the `block` dependency. Passing automated checks is not a substitute for the author's UI review.
