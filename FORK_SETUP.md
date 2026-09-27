# Personal fork setup

Keep this file and `.pi/APPEND_SYSTEM.md` exclusively on `fork/agent-instructions`. Never merge that branch into code branches.

## Branches

| Branch | Purpose | Base |
| --- | --- | --- |
| `main` | Clean upstream baseline | `upstream/main` |
| `feature/<name>` | One upstream contribution | Fresh `main` |
| `dev` | Combined personal build | Merge `main` and selected features |
| `fork/agent-instructions` | Portable agent setup | Separate maintenance branch |

An unpublished dependency requires a stack: branch from that specific feature, not `dev`. Keep personal-only changes on separate branches and integrate them into `dev` too.

## New computer

```sh
git clone https://github.com/Quadriphobs1/zed.git
cd zed
git remote add upstream https://github.com/zed-industries/zed.git
git config remote.pushDefault origin
git config push.default simple
git config pull.ff only
git fetch upstream main
git branch --set-upstream-to=upstream/main main
git switch main
git merge --ff-only upstream/main
```

Stop if `main` diverges; never reset it automatically. Once `dev` has been published, check it out with `git switch --track origin/dev`. If already local, use `git switch dev`. If it has not been published, create it from `main`, then merge the selected feature branches.

## Bootstrap or refresh instructions

Run from each code worktree before starting its agent. This fetches the canonical branch, backs up a differing local copy outside the checkout, and installs the ignored instructions. It refuses to overwrite a tracked instruction file. Inspect any backup and reconcile personal edits before reloading.

```sh
(
    set -eu
    cd "$(git rev-parse --show-toplevel)"
    if git ls-files --error-unmatch .pi/APPEND_SYSTEM.md >/dev/null 2>&1; then
        printf '%s\n' 'Refusing to replace tracked instructions.' >&2
        exit 1
    fi

    instructions_ref=origin/fork/agent-instructions
    if [ "$instructions_ref" = origin/fork/agent-instructions ]; then
        printf '%s\n' "[$(date '+%H:%M:%S')] Fetch fork instructions"
        git fetch origin fork/agent-instructions
    fi
    temporary=$(mktemp)
    trap 'rm -f "$temporary"' EXIT
    git show "$instructions_ref":.pi/APPEND_SYSTEM.md > "$temporary"
    test -s "$temporary"

    if [ -e .pi/APPEND_SYSTEM.md ] && ! cmp -s .pi/APPEND_SYSTEM.md "$temporary"; then
        backup=$(mktemp "${TMPDIR:-/tmp}/zed-fork-instructions.XXXXXX")
        cp .pi/APPEND_SYSTEM.md "$backup"
        printf 'Previous instructions saved to %s\n' "$backup"
    fi

    mkdir -p .pi
    exclude=$(git rev-parse --git-path info/exclude)
    mkdir -p "$(dirname "$exclude")"
    touch "$exclude"
    grep -qxF '/.pi/APPEND_SYSTEM.md' "$exclude" || printf '\n/.pi/APPEND_SYSTEM.md\n' >> "$exclude"
    cp "$temporary" .pi/APPEND_SYSTEM.md
    git check-ignore .pi/APPEND_SYSTEM.md
    printf '%s\n' "[$(date '+%H:%M:%S')] Installed; review changes and reload Pi"
)
```

For reviewed but unpublished guidance on this machine, change the assignment above to `instructions_ref=fork/agent-instructions`. This skips fetching and requires that local branch to exist. The default remote mode always fetches; it does not move the local maintenance branch.

Start Pi from the worktree root, grant project trust if prompted, and run `/reload` in existing sessions. Pi loads `.pi/APPEND_SYSTEM.md` alongside upstream `AGENTS.md`; a project append replaces a user-level append, so reconcile them if both exist. Other agent tools need their own loading setup.

Ignored files survive branch switches but are not copied into new worktrees. Remote guidance updates do not update installed copies automatically. Before each task, fetch and compare the canonical file; repeat bootstrap if it differs. Backups under the system temporary directory are temporary: preserve any needed edits elsewhere.

## Start an independent feature

Keep the original checkout on `dev` for the daily-use build. With a clean checkout and no other worktree owning `main`:

```sh
git fetch upstream main
git switch main
git merge --ff-only upstream/main
git switch dev
git worktree add -b feature/example ../zed-example main
```

If another worktree owns `main`, fast-forward it there instead. Install instructions in `../zed-example` before starting the agent. Implement, test, and obtain two independent subagent reviews before committing. A customized Zed binary can edit this independent checkout.

After the feature is reviewed and committed, integrate it from the clean `dev` worktree:

```sh
git merge --no-ff feature/example
```

Test/build the combined app. Make subsequent feature fixes on `feature/example` and merge them into `dev` again. Keep integration-only conflict resolutions on `dev`; never merge `dev` back into a contribution branch.

## Sync upstream

1. Fetch upstream and fast-forward `main` in its owning worktree.
2. Merge `main` into active feature branches in their worktrees. Avoid rewriting published history.
3. In the clean `dev` worktree, merge `main`, then updated feature branches.
4. Review integration changes and run relevant tests/builds.
5. Push approved branches only after explicit user approval. Local commits are not a remote backup.

Do not reset or force-push to resolve divergence. Decide how to reconcile it with the user.

## Maintain guidance

Use a separate worktree to avoid replacing ignored copies while switching to a branch that tracks the same path:

```sh
git worktree add ../zed-agent-instructions fork/agent-instructions
```

If the branch is not local yet, first fetch it and create it with `git branch --track fork/agent-instructions origin/fork/agent-instructions`. If its worktree already exists, use that worktree.

Edit only personal setup files there, review, and commit. Publish with approval, then repeat bootstrap in every active code worktree and reload Pi. Until published, explicitly bootstrap from the local branch. Never merge the instruction branch into `dev` or a contribution branch.

## Before an upstream PR

```sh
git fetch upstream main
git log --oneline upstream/main..HEAD
git diff --stat upstream/main...HEAD
git diff upstream/main...HEAD -- .pi/APPEND_SYSTEM.md FORK_SETUP.md
```

The last command must produce no output. Review the complete diff and relevant checks. For stacked work, use the prerequisite branch to inspect the dependent change and explain the dependency; wait for its merge or agree on stacked review with maintainers.

Push only approved feature branches to the fork. For an independent contribution, explicitly target `zed-industries/zed:main`, never `dev`. Follow upstream contribution rules and keep the README review marker until the human removes it.

Exclusions are convenience, not enforcement: tracked files and `git add -f` bypass them.
