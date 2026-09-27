# Personal Zed fork workflow

Follow upstream AGENTS.md and crate instructions as well as this guidance.

## Branch roles

- `origin`: https://github.com/Quadriphobs1/zed; `upstream`: https://github.com/zed-industries/zed.git. Verify both before syncing or publishing.
- `main` is the clean upstream baseline, tracking `upstream/main`. Fast-forward only; never implement features here.
- `dev` is the daily-use integration branch. Merge selected feature branches here and build the combined app. Never use it as the base for an upstream contribution or merge it back into a feature branch.
- `feature/<name>` contains one reviewable contribution, based on freshly synced `main`. Personal-only changes also get separate branches, integrated into `dev`.
- If work depends on an unpublished feature, branch from that specific feature and record the dependency. Treat it as a stacked contribution, not an independent PR.
- `fork/agent-instructions` holds personal setup only. Never merge or cherry-pick it into `main`, `dev`, or feature branches.

## Start work

1. Inspect status, branch, worktrees, remotes, and this instruction copy. Stop on unexpected changes; never automatically reset, stash, or discard work.
2. Before a new task, fetch `origin fork/agent-instructions` and compare its `.pi/APPEND_SYSTEM.md` with the local copy. If different, read the updated `FORK_SETUP.md`, preserve local edits, reinstall, and reload Pi before implementation. If fetching fails, report that freshness is unverified.
3. Fetch upstream and fast-forward `main` in its owning worktree. If no worktree owns `main`, update it from a clean checkout, then return to `dev`. Stop on divergence.
4. For an independent feature, create a separate worktree from `main`, using `git worktree add -b feature/<name> ../zed-<name> main`. Install the ignored instructions in that worktree before starting its agent.
5. Implement and test only the feature there. The running customized Zed can edit this worktree; it need not be built from the checkout being edited.
6. Complete at least two independent subagent reviews before committing changes. Fix findings and run relevant checks; report actual evidence and gaps.
7. Merge the reviewed feature into `dev` in the integration worktree, then test/build the combined app. Resolve integration-only conflicts there; put feature fixes on the owning feature branch and merge them forward again.

## Sync and publish

- Fetch upstream, fast-forward `main`, merge it into active published feature branches, then merge `main` and updated features into `dev`. Never merge `dev` back into those branches.
- Prefer merges for published branches. Ask before rewriting history; never use plain force-push.
- Never push, including `main`, `dev`, and instruction updates, without explicit user approval. Default pushes target `origin`; pulls are fast-forward-only.
- Do not open upstream PRs automatically. Follow upstream contribution requirements and obtain user approval.

## Instruction installation

- Canonical guidance lives on `fork/agent-instructions`; `FORK_SETUP.md` contains the bootstrap command. Agents cannot read instructions merely because another branch contains them.
- Each code worktree needs an ignored local `.pi/APPEND_SYSTEM.md`. Branch switches retain that copy, but linked worktrees do not inherit it. Updates are not automatic.
- Start Pi from the worktree root, grant project trust, and reload existing sessions after refreshing. A project append file replaces a user-level append file; reconcile both if needed. Other agent tools require their own instruction-loading setup.
- Edit canonical guidance in a dedicated `fork/agent-instructions` worktree. Commit only setup files, publish with approval, and refresh every active code worktree. For unpublished guidance, bootstrap from the reviewed local branch explicitly.
- Never force-add local copies. Exclusions do not protect tracked files. Preserve upstream `.rules`, `AGENTS.md`, and `CLAUDE.md` rather than placing personal workflow in them.

## Before an upstream PR

- Fetch upstream; inspect all commits and the full diff against the intended base, normally `upstream/main`.
- Confirm no personal setup commits, `.pi/APPEND_SYSTEM.md`, `FORK_SETUP.md`, or unrelated customizations appear.
- For a stack, identify the prerequisite and review only the intended dependent change. Do not present the combined stack as an independent contribution.
- Run relevant checks, follow the upstream PR template, and retain the mandatory README review marker until the human removes it.
- Push only approved branches to `origin`. Target `zed-industries/zed:main` explicitly for an independent upstream PR, never `dev`.
