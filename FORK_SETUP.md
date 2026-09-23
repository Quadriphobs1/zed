# Personal fork setup

This document and `.pi/APPEND_SYSTEM.md` belong only on `fork/agent-instructions`. Never merge this branch into a contribution branch.

## New computer

First publish `fork/agent-instructions` to `origin` from the original computer, with explicit approval. Local commits alone are not a remote backup.

Clone and configure:

```sh
git clone https://github.com/Quadriphobs1/zed.git
cd zed
git remote add upstream https://github.com/zed-industries/zed.git
git config remote.pushDefault origin
git config push.default simple
git config pull.ff only
git fetch upstream main
git branch --set-upstream-to=upstream/main main
git merge --ff-only upstream/main
```

Stop if `main` has diverged; inspect rather than reset.

Install the instructions without adding them to your branch:

```sh
git fetch origin fork/agent-instructions
git branch --track fork/agent-instructions origin/fork/agent-instructions
mkdir -p .pi
# Refuse to overwrite an existing local instruction file.
(set -C; git show origin/fork/agent-instructions:.pi/APPEND_SYSTEM.md > .pi/APPEND_SYSTEM.md)
exclude=$(git rev-parse --git-path info/exclude)
grep -qxF '/.pi/APPEND_SYSTEM.md' "$exclude" || printf '\n/.pi/APPEND_SYSTEM.md\n' >> "$exclude"
git check-ignore .pi/APPEND_SYSTEM.md
git status --short
```

Skip branch creation if it already exists. If extraction fails, inspect the local file before retrying. Never force-add the instruction file on a contribution branch.

Start Pi from the checkout root, grant project trust if prompted, and run `/reload` in existing sessions. Pi loads the local `.pi/APPEND_SYSTEM.md` alongside upstream's `AGENTS.md`. A project append file takes precedence over a user-level `APPEND_SYSTEM.md`; reconcile them manually if you use both. Other agent tools need their own local instruction-loading setup.

Repeat instruction installation for each linked worktree. Remotes and repository config are shared by linked worktrees; the untracked instruction file is not.

## Daily workflow

With a clean checkout:

```sh
git switch main
git pull --ff-only
git switch -c feature/<name>
```

Keep `main` free of custom changes. On an unpublished feature branch, fetch upstream and rebase onto `upstream/main` as needed. Coordinate before rewriting published branches.

Updating GitHub's fork-main is optional and requires approval:

```sh
git push origin main
```

## Update personal guidance

With a clean checkout, switch to `fork/agent-instructions`. The ignored local copy can be overwritten by checkout, so preserve any local edits first. Edit and commit `.pi/APPEND_SYSTEM.md` and, if needed, this guide. Do not edit upstream source on this branch.

Return to your contribution branch, then restore the committed instructions:

```sh
mkdir -p .pi
git show fork/agent-instructions:.pi/APPEND_SYSTEM.md > .pi/APPEND_SYSTEM.md
```

Inspect and preserve any existing local changes before running that command. Reload Pi. Publishing updates requires explicit approval:

```sh
git push -u origin fork/agent-instructions
```

## Before an upstream PR

```sh
git fetch upstream main
git log --oneline upstream/main..HEAD
git diff --stat upstream/main...HEAD
git diff upstream/main...HEAD -- .pi/APPEND_SYSTEM.md FORK_SETUP.md
```

The last command must produce no output. Also inspect the complete diff and run relevant checks. Push only the approved feature branch to the fork; explicitly target `zed-industries/zed:main` for the PR.

Exclusions are convenience, not enforcement: `git add -f` and tracked files bypass them. Branch separation and final diff review keep personal setup out of upstream PRs.
