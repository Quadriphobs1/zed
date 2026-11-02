# Pull request browser prototype

This fork adds a read-only **Pull Requests** tab to the Git panel. Open it directly with **git panel: activate pull requests tab** in the command palette.

## Scope and prerequisites

- Open a **trusted local project** with a GitHub remote recognized by Zed's hosting-provider registry, including GitHub Enterprise. Remote URLs and host settings use the existing Git integration; SSH/remote **projects** and non-GitHub providers remain unsupported. SSH Git URLs and HTTPS URLs with usernames are supported.
- Install GitHub CLI (`gh`) on Zed's executable search path and authenticate outside Zed (`gh auth login --hostname HOST`, then `gh auth status --hostname HOST`). PR data still requires the host's API; local Git history and SSH credentials alone cannot supply it. Zed reuses the CLI session without initiating login.
- The only GitHub remote is selected automatically. With several, the browser restores the last selection for this local repository, or selects the first remote alphabetically. `origin` may be your fork; select `upstream` to browse the upstream repository. **Reload remotes** rereads configuration and preserves the selection if its name and GitHub target still match; otherwise it selects the first recognized GitHub remote. The choice is stored locally, not in Git.
- Lists contain at most **100 open PRs**. This is a bounded prototype, not a complete/paginated listing. Use GitHub for additional results.
- The selected remote's default list preloads while the Git panel initializes, after workspace trust checks. Lists and opened descriptions use a bounded, in-memory session cache, scoped by local repository, GitHub target and filter/PR. Snapshots are fresh for 60 seconds; stale snapshots remain visible while updating. **Refresh** bypasses freshness. Requests are shared, time out after 30 seconds, and errors clear displayed cached content. There is no polling, disk cache of PR contents, bulk description loading, or background Git-object fetching.
- PR details use a native header and action bar above a separately scrolling description, on the themed editor background. **Refresh** reloads metadata; **View changes** opens the native preview; the **GitHub** menu opens the published PR or diff. **Details** expands the pinned commit IDs (with copy buttons), snapshot notice and fetch guarantees. The description uses Zed's Markdown-preview font settings without resizing native controls, project languages for code highlighting, bounded reading width, selection/copy menus, and code-block copy/wrap controls. Comments and review threads are not loaded.
- **View changes** opens a read-only merge-base-to-head snapshot of the description's pinned commits, including in shallow checkouts. Already available history is used only when its merge base can be certified: every reachable shallow boundary must be at or below that base. Otherwise an isolated temporary repository fetches the pinned base and the base repository's PR reference using `gh` credentials, at depths **32, 128, then 512** as needed. It requests blobless history, then downloads only missing blobs for changed files; servers that ignore filtering may transfer more data. Depth bounds ancestry, not bytes or total commits on merged branches. The checkout's objects, shallow boundary, refs, `FETCH_HEAD`, configuration, index and working files are unchanged. No checkout, staging, hooks, submodule fetching or automatic maintenance occurs. Git snapshot preparation has a **two-minute** deadline; dropping the request cancels it. Temporary storage is removed when the request ends. If the bounded history cannot certify a unique merge base, use **GitHub → Open diff on GitHub** rather than unshallowing the checkout. Unavailable pinned revisions require refreshing the description, for example after a force-push.
- **GitHub → Open diff on GitHub** opens the current published file changes in your browser. Native previews never substitute a local branch/worktree diff for a published PR diff. The separate **Compare HEAD with Upstream (No Fetch)** feature still only compares locally available committed revisions.

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
2. With one recognized GitHub remote, verify PRs load without a click. With both a fork `origin` and upstream remote, verify the alphabetically first remote loads initially. Select the other remote, reload remotes, reopen the repository, then restart Zed; verify the selection is restored each time. Remove or retarget the saved remote and confirm fallback to the first recognized GitHub remote; unsupported remotes must not be selected. Confirm repository names and PRs differ as expected. Switch active local repositories during a slow request; preferences must remain repository-specific and old results/errors must not appear in the new repository.
3. Exercise **All**, **Assigned**, **Review requested**, and **Created by me**. Compare to GitHub while signed into the same account as `gh`. Rapidly switch filters/remotes and refresh; old results/errors must not replace the current selection. Check loading, empty, and explicit 100-result-limit messages. Arrow keys select rows and Enter opens a description. Resize the panel: titles and authors should stay left-aligned and truncate, and clicking either line should open the PR. Rows use History's virtualized-list pattern.
4. Open a PR. Check number, title, author (including deleted users), draft/state and branch names in the native header. Expand **Details**, verify full base/head IDs via hover and copy buttons, then collapse it. Scroll a long description: header/actions must remain visible. Resize to a narrow split pane and a wide window; long titles/branch names must not overflow, actions must wrap, and the pane background must remain continuous. Include headings, lists, tables, links, fenced code, long lines and an empty description; check light/dark themes, selection and right-click copy/link menus, code highlighting, copy/wrap controls and scrolling. Change `markdown_preview.font_size`: description typography should change without resizing header/actions. Check both **GitHub** menu destinations. Refresh after an edit or force-push: metadata is a snapshot, whereas browser links always show current GitHub state. Closed/merged PRs opened from a stale list should show their current state. During loading/fetch errors, ensure the header still shows retry controls and the error (hover for full text).
5. With `gh` absent from PATH, unauthenticated, denied repository access, offline, or rate limited, confirm an actionable visible error and a working retry after repair. No interactive login should open. Avoid logging credentials or changing your normal account merely to simulate failures.
6. In a restricted workspace, confirm no PR CLI requests occur and a trust message is visible. Trust the workspace through Zed's existing controls, then reload remotes. Try an SSH/remote project, an unsupported remote host, and a repository with no remotes; each should show an explicit unsupported/empty state rather than running a local command against a remote path.
7. Leave the Git panel on Changes/History while the trusted repository initializes, then open Pull Requests; check that prefetched results appear without restarting a request. Revisit lists and descriptions within 60 seconds, then after expiry; cached content should remain visible during revalidation. Explicit Refresh must revalidate immediately. Switch repositories, remotes and filters during delayed responses: snapshots must not cross targets. Close a description during a request, reopen it, and confirm request sharing/completion. Repeat offline and repair connectivity; failed requests must be retryable.
8. With staged and unstaged edits present, open **View changes** for a PR whose objects are absent locally, including a fork PR. Confirm the viewer matches GitHub's merge-base diff for the pinned head, supports normal read-only diff navigation, and excludes local edits. Refresh metadata after a force-push; no silently substituted newer head is allowed. Repeat in a depth-one checkout whose base and head have diverged; confirm the correct merge-base snapshot, not a direct base-to-head diff. Test ancestry beyond the bounded limit, an unavailable head, absent `gh` credentials and a failed fetch; expect actionable errors and the GitHub fallback. Refresh or close the description during loading; obsolete work must not open an unrelated preview.
9. Before and after browsing and explicit diff previews, compare `git status --porcelain=v1`, HEAD, refs, index/config/`FETCH_HEAD`/shallow-file bytes, staged/unstaged edits, untracked files and the checkout's object database. List/description browsing must not fetch Git objects. Only an explicit native-preview action may fetch into temporary storage; neither path may add objects to the checkout, unshallow it, checkout a branch, post reviews, write GitHub data, change refs or alter the working tree/index. Close or refresh during ancestry/blob fetching; confirm cancellation releases the repository queue and no stale diff tab appears.

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

Historical prototype verification on macOS with Rust 1.98.1 (not proof of subsequent changes):

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
