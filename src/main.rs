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

fn main() -> Result<()> {
    let args = Args::parse();
    let sh = Shell::new()?;
    ensure_tool(&sh, "git")?;
    ensure_tool(&sh, "gh")?;

    let current_branch = cmd!(sh, "git branch --show-current")
        .quiet()
        .read()
        .context("getting current branch")?;

    let branch = match (args.branch_or_pr_number, current_branch.as_str()) {
        (None, "main") => bail!("on main; must specify the PR number or branch name to merge"),
        (None, _) => current_branch,
        (Some(branch), _) => {
            if branch.parse::<u64>().is_ok() {
                let json = cmd!(sh, "gh pr view {branch} --json headRefName")
                    .quiet()
                    .read()
                    .context("getting branch name for pr number")?;
                let value =
                    serde_json::from_str::<Value>(&json).context("parsing gh branch name")?;
                let Some(branch) = value.pointer("/headRefName").and_then(Value::as_str) else {
                    bail!("github did not return headRefName in {json}");
                };
                branch.to_owned()
            } else {
                branch
            }
        }
    };

    // get review and current ci status
    let status = cmd!(
        sh,
        "gh pr view {branch} --json baseRefName,reviewDecision,statusCheckRollup"
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

    // ensure that the branch is at the tip of its base for a linear history
    let base = status.base_ref_name;
    cmd!(sh, "git fetch").run().context("git fetch")?;
    cmd!(sh, "git checkout {branch}")
        .run()
        .context("git checkout branch")?;

    let remote = args.remote.as_str();

    // Before we rebase, make sure that the state on the local branch corresponds to the one on
    // remote. Local branch state could differ if there was already a branch that wasn't in sync
    // with the remote. In this case we don't want to do a rebase and `push -f` as that would
    // overwrite the remote branch and merge local state, instead of remote.
    if !local_branch_matches_remote(&sh, remote, &branch)? {
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
    if !local_branch_matches_remote(&sh, remote, &branch)? {
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
