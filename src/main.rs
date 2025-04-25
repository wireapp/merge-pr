use std::borrow::Cow;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use serde_json::Value;
use xshell::{cmd, Shell};

/// Merge this pull request, ensuring a linear history.
///
/// Github's rebase-and-merge button doesn't fast-forward properly.
/// This tool does it better.
#[derive(Debug, Parser)]
struct Args {
    /// Branch name or PR number to merge
    ///
    /// Accepts 3 formats: a PR number, the name of a branch on the remote, or `<fork-owner>:<fork-branch-name>`.
    branch_or_pr_number: Option<String>,

    /// When set, ignore CI and just merge straightaway
    #[arg(long)]
    ignore_ci: bool,

    /// How long to wait (seconds) between push attempts.
    ///
    /// This program will retry the final push of to the base exactly once,
    /// after this interval, in order to ensure that github has the chance
    /// to synchronize itself.
    #[arg(short = 'i', long, default_value_t = 2.5)]
    push_retry_interval: f64,

    /// When set, perform checks but do not actually change the repo state.
    #[arg(short, long)]
    dry_run: bool,

    /// When set, retain the merged branch instead of deleting it locally.
    #[arg(short, long)]
    retain_branch: bool,

    /// Name of the relevant git remote.
    #[arg(short = 'R', long, default_value = "origin")]
    remote: String,
}

fn ensure_tool(sh: &Shell, tool_name: &str) -> Result<()> {
    cmd!(sh, "which {tool_name}")
        .quiet()
        .ignore_stdout()
        .run()
        .map_err(|_| anyhow!("tool `{tool_name}` is required"))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckRun {
    // we do want to deserialize the name
    #[allow(dead_code)]
    name: String,
    status: Option<String>,
    conclusion: String,
}

impl CheckRun {
    fn is_successy(&self) -> bool {
        self.status.as_deref() == Some("COMPLETED")
            && (self.conclusion == "SUCCESS" || self.conclusion == "SKIPPED")
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "__typename")]
enum StatusCheck {
    CheckRun(CheckRun),
    // we don't care about the value here, but serde needs to know to deserialize _something_
    #[allow(dead_code)]
    StatusContext(Value),
}

impl StatusCheck {
    fn as_check_run(&self) -> Option<&CheckRun> {
        match self {
            Self::CheckRun(check_run) => Some(check_run),
            _ => None,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    base_ref_name: String,
    review_decision: String,
    status_check_rollup: Vec<StatusCheck>,
}

impl Status {
    fn is_approved(&self) -> bool {
        self.review_decision == "APPROVED"
    }
}

fn local_branch_matches_remote(sh: &Shell, remote: &str, branch: &str) -> Result<bool> {
    let branch_sha = cmd!(sh, "git rev-parse {branch}")
        .read()
        .context("reading branch sha")?;
    let remote_branch_sha = cmd!(sh, "git rev-parse {remote}/{branch}")
        .read()
        .context("reading remote branch sha")?;
    Ok(branch_sha == remote_branch_sha)
}

fn repo_owner_login(sh: &Shell) -> Result<String> {
    let json = cmd!(sh, "gh repo view --json owner")
        .quiet()
        .read()
        .context("getting repo owner name")?;
    let value = serde_json::from_str::<Value>(&json).context("parsing gh repo owner name")?;
    let login = value
        .pointer("/owner/login")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("malformed result when getting gh repo owner"))?;
    Ok(login.into())
}

struct PrData {
    fork_owner: Option<String>,
    branch: String,
}

impl PrData {
    fn new(fork_owner: Option<&str>, branch: &str) -> Self {
        Self {
            fork_owner: fork_owner.map(ToOwned::to_owned),
            branch: branch.to_owned(),
        }
    }

    fn from_branch(branch: &str) -> Self {
        Self {
            fork_owner: None,
            branch: branch.into(),
        }
    }

    /// Parse a branch or PR number into `Self`
    ///
    /// Accepts 3 formats:
    ///
    /// - `<integer>`: a PR number
    /// - `<string>`: a branch on the current remote
    /// - `<string>:<string>`: the owner of a fork, followed by the branch on that fork
    fn parse(sh: &Shell, branch_or_pr_number: &str) -> Result<Self> {
        if branch_or_pr_number.parse::<u64>().is_ok() {
            let owner = repo_owner_login(sh)?;
            let number = branch_or_pr_number;
            let json = cmd!(
                sh,
                "gh pr view {number} --json headRefName,headRepositoryOwner"
            )
            .quiet()
            .read()
            .context("getting branch name for pr number")?;
            let value = serde_json::from_str::<Value>(&json).context("parsing gh branch name")?;
            let branch = value
                .pointer("/headRefName")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("github did not return headRefName in {json}"))?;
            let head_owner = value
                .pointer("/headRepositoryOwner/login")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("malformed response getting head repository owner"))?;
            let fork_owner = (owner != head_owner).then_some(head_owner);
            Ok(Self::new(fork_owner, branch))
        } else if let Some((fork_owner, branch)) = branch_or_pr_number.split_once(':') {
            Ok(Self::new(Some(fork_owner), branch))
        } else {
            Ok(Self::from_branch(branch_or_pr_number))
        }
    }

    fn qualified_branch(&self) -> Cow<str> {
        if let Some(fork_owner) = self.fork_owner.as_deref() {
            format!("{fork_owner}:{}", self.branch).into()
        } else {
            (&self.branch).into()
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let sh = Shell::new()?;
    ensure_tool(&sh, "git")?;
    ensure_tool(&sh, "gh")?;

    let current_branch = cmd!(sh, "git branch --show-current")
        .quiet()
        .read()
        .context("getting current branch")?;

    let pr_data = match (args.branch_or_pr_number, current_branch.as_str()) {
        (None, "main") => bail!("on main; must specify the PR number or branch name to merge"),
        (None, _) => PrData::from_branch(&current_branch),
        (Some(branch), _) => PrData::parse(&sh, &branch)?,
    };

    let branch = &pr_data.branch;
    let qualified_branch = pr_data.qualified_branch();
    let qualified_branch = qualified_branch.as_ref();

    // get review and current ci status
    let status = cmd!(
        sh,
        "gh pr view {qualified_branch} --json baseRefName,reviewDecision,statusCheckRollup"
    )
    .quiet()
    .read()
    .context("getting status from github")?;

    let status = serde_json::from_str::<Status>(&status).context("parsing github status")?;
    if !status.is_approved() {
        bail!("{branch} has not been approved");
    }

    if !args.ignore_ci {
        let non_success = status
            .status_check_rollup
            .iter()
            .filter_map(StatusCheck::as_check_run)
            .filter(|check_run| !check_run.is_successy())
            .collect::<Vec<_>>();
        if !non_success.is_empty() {
            for check_run in non_success {
                println!("{check_run:?}");
            }
            bail!("some ci checks are incomplete or unsuccessful");
        }
    }

    if args.dry_run {
        println!("all checks OK but aborting due to dry run");
        return Ok(());
    }

    let remote = args.remote.as_str();

    // ensure that the branch is at the tip of its base for a linear history
    let base = status.base_ref_name;
    cmd!(sh, "git fetch {remote}").run().context("git fetch")?;
    cmd!(sh, "git checkout {branch}")
        .run()
        .context("git checkout branch")?;

    // Before we rebase, make sure that the state on the local branch corresponds to the one on
    // remote. Local branch state could differ if there was already a branch that wasn't in sync
    // with the remote. In this case we don't want to do a rebase and `push -f` as that would
    // overwrite the remote branch and merge local state, instead of remote.
    if !local_branch_matches_remote(&sh, remote, branch)? {
        bail!("local branch {branch} differs from remote branch {remote}/{branch}");
    }

    let rebase_result = cmd!(sh, "git rebase {remote}/{base}").run();
    if rebase_result.is_err() {
        cmd!(sh, "git rebase --abort")
            .run()
            .context("aborting rebase")?;
        bail!("{branch} did not cleanly rebase onto {remote}/{base}; do so manually and try again");
    }

    // if rebase moved the tip then force-push to ensure github is tracking the new history
    // this resets CI, but doesn't mess with the approvals. We can assume CI is OK, at this point
    if !local_branch_matches_remote(&sh, remote, branch)? {
        cmd!(sh, "git push -f {remote} {branch}")
            .run()
            .context("force-pushing branch")?;
    }

    // we can now actually merge this to main without breaking anything
    cmd!(sh, "git checkout {base}")
        .run()
        .context("checking out base")?;
    cmd!(sh, "git merge {branch} --ff-only")
        .run()
        .context("performing ff-only merge to base")?;

    // in principle we can now just push; github has some magic to ensure that if you are pushing main
    // to a commit which is at the tip of an approved pr, then it counts it as a manual merge operation
    // and is permitted.
    //
    // sometimes it takes a few seconds for github to catch up, so in the event of a failure we try again
    // a bit later.
    let push_result = cmd!(sh, "git push {remote} {base}").run();
    if push_result.is_err() {
        println!("this is normal; retrying in {}s", args.push_retry_interval);
        std::thread::sleep(std::time::Duration::from_secs_f64(args.push_retry_interval));
        cmd!(sh, "git push {remote} {base}")
            .run()
            .context("2nd attempt to push to base")?;
    }

    if !args.retain_branch {
        cmd!(sh, "git branch -D {branch}")
            .run()
            .context("removing merged branch")?;
    }

    Ok(())
}
