# Personal Zed fork workflow

Follow upstream AGENTS.md and crate instructions as well as this guidance.

## Branch roles

- `origin`: https://github.com/Quadriphobs1/zed; `upstream`: https://github.com/zed-industries/zed.git. Verify both before syncing or publishing.
- `main` is the clean upstream baseline, tracking `upstream/main`. Fast-forward only; never implement features here.
- `dev` is the daily-use integration branch. Merge selected feature branches here and build the combined app. Never use it as the base for an upstream contribution or merge it back into a feature branch.
- `feature/<name>` contains one reviewable contribution, based on freshly synced `main`. Personal-only changes also get separate branches, integrated into `dev`.
- If work depends on an unpublished feature, branch from that specific feature and record the dependency. Treat it as a stacked contribution, not an independent PR.
- `fork/agent-instructions` holds personal setup only. Never merge or cherry-pick it into `main`, `dev`, or feature branches.
- Use one checkout and switch branches by default. Minimize worktrees; ask before creating an extra checkout. Do not run concurrent writers while switching branches.

## Start work

1. Inspect status, branch, worktrees, remotes, and this instruction copy. Stop on unexpected changes; never automatically reset, stash, or discard work.
2. Before a new task, fetch `origin fork/agent-instructions` and compare it with the local guidance branch and installed copy. Preserve reviewed unpublished local guidance; do not replace it with an older remote version. On divergence or unknown local edits, ask before reconciling. Read the selected branch's `FORK_SETUP.md`, reinstall if needed, and reload Pi. If fetching fails, report that freshness is unverified.
3. From a clean checkout, fetch upstream, switch to `main`, and run `git merge --ff-only upstream/main`. Stop on divergence.
4. For an independent feature, run `git switch -c feature/<name> main` in this same checkout. Resume existing work by switching to its feature branch instead.
5. Implement and test only that feature. A running customized Zed can edit this checkout even though its source branch has changed. Rebuilding replaces the shared build output; return to `dev` to rebuild the combined app.
6. Complete at least two independent subagent reviews before committing changes. Fix findings and run relevant checks; report actual evidence and gaps.
7. With a clean checkout, switch to `dev`, merge the reviewed feature, then test/build the combined app. Resolve integration-only conflicts there; put feature fixes on the owning feature branch and merge them forward again.

## Sync and publish

- Fetch upstream, fast-forward `main`, merge it into active published feature branches, then merge `main` and updated features into `dev`. Never merge `dev` back into those branches.
- Prefer merges for published branches. Ask before rewriting history; never use plain force-push.
- Never push, including `main`, `dev`, and instruction updates, without explicit user approval. Default pushes target `origin`; pulls are fast-forward-only.
- Do not open upstream PRs automatically. Follow upstream contribution requirements and obtain user approval.

## Instruction installation

- Canonical guidance lives on `fork/agent-instructions`; `FORK_SETUP.md` contains the bootstrap command. Agents cannot read instructions merely because another branch contains them.
- Each code worktree needs an ignored local `.pi/APPEND_SYSTEM.md`. Branch switches retain that copy, but linked worktrees do not inherit it. Updates are not automatic.
- Start Pi from the worktree root, grant project trust, and reload existing sessions after refreshing. A project append file replaces a user-level append file; reconcile both if needed. Other agent tools require their own instruction-loading setup.
- To edit canonical guidance, first preserve the ignored copy outside the checkout, then switch to `fork/agent-instructions`. This branch tracks the same path, so checkout can replace it. Commit only setup files, switch back, and reinstall the reviewed local instructions immediately: switching away removes the tracked file. Publish only with approval.
- Never force-add local copies. Exclusions do not protect tracked files. Preserve upstream `.rules`, `AGENTS.md`, and `CLAUDE.md` rather than placing personal workflow in them.

## Before an upstream PR

- Fetch upstream; inspect all commits and the full diff against the intended base, normally `upstream/main`.
- Confirm no personal setup commits, `.pi/APPEND_SYSTEM.md`, `FORK_SETUP.md`, or unrelated customizations appear.
- For a stack, identify the prerequisite and review only the intended dependent change. Do not present the combined stack as an independent contribution.
- Run relevant checks, follow the upstream PR template, and retain the mandatory README review marker until the human removes it.
- Push only approved branches to `origin`. Target `zed-industries/zed:main` explicitly for an independent upstream PR, never `dev`.
