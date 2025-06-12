# `merge-pr`: A Github merge button which preserves linear history and commit signing

Want a linear git history? Github has a button for you: "Rebase and Merge".
You can even disable all the other merge buttons and force its use!

Problem is, Github is not particularly smart about how it does this.
In particular, if you have signed commits and your branch is showing up proud and verified,
your `main` will still have unverified commits. Github really _means_ rebase, in that case,
even when `git merge --ff-only` would do the job.

With this tool you have a "merge button" which you can run to merge a PR from the command line,
preserving both linear history and your signed commits.

## Peer dependencies

This tool will fail unless both `git` and `gh` are installed, available on the PATH, and configured appropriately.

## Installation

### With Rust Toolchain / Local Build

```sh
cargo install --git https://github.com/wireapp/merge-pr
```

### Without Rust Toolchain / Download Prebuilt Artifact

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/wireapp/merge-pr/releases/latest/download/merge-pr-installer.sh | sh
```
