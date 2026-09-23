# Personal Zed fork workflow

These instructions apply to Quadriphobs1/zed. Keep following the repository's AGENTS.md and crate-specific instructions.

## Remotes and branches

- `origin` is https://github.com/Quadriphobs1/zed; `upstream` is https://github.com/zed-industries/zed.git. Verify both before syncing or publishing.
- Keep local `main` an untouched upstream baseline, tracking `upstream/main`. Default pushes target `origin`; pulls are fast-forward-only.
- Before syncing, inspect status and branch. With a clean checkout on `main`, fetch upstream and run `git merge --ff-only upstream/main`. Stop on divergence; never reset, stash, or discard work automatically.
- Start contribution branches from freshly synced `main`, one reviewable change per branch. Never implement features on `main` or `fork/agent-instructions`.
- Keep personal-only customizations on separate branches. Never merge them into upstream contribution branches.
- Rebase unpublished feature branches onto `upstream/main` when needed. Ask before rewriting published history; never use plain force-push.
- Never push, including fork-main synchronization, without explicit user approval. Commit locally and report first.

## Personal instructions

- The canonical instruction file is `.pi/APPEND_SYSTEM.md` on `fork/agent-instructions`. That branch also contains `FORK_SETUP.md` with restoration instructions.
- On contribution branches, this file is an untracked local copy excluded through `.git/info/exclude`. Git exclusions do not protect files already tracked or force-added.
- Never merge or cherry-pick `fork/agent-instructions` into an upstream PR branch. Never force-add the local instruction copy.
- Update the canonical file on its dedicated branch, commit only personal setup files there, then refresh the ignored copy on the working branch. Check for existing local changes before switching or replacing files; use a separate worktree when needed.
- Preserve upstream `.rules`, `AGENTS.md`, and `CLAUDE.md`; do not put personal fork workflow in them.
- For a new linked worktree, install the ignored instruction copy there too. Do not assume it is copied automatically.

## Before an upstream PR

- Fetch upstream. Review all commits and the full diff against the intended base, normally `upstream/main`.
- Confirm the PR branch has no personal setup commits, `.pi/APPEND_SYSTEM.md`, `FORK_SETUP.md`, or unrelated fork customizations.
- Run relevant repository checks and report actual evidence and any gaps. Follow the upstream PR template and contribution requirements.
- Push only the approved feature branch to `origin`. Target `zed-industries/zed:main` explicitly when creating an upstream PR.
