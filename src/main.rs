use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use itertools::Itertools as _;
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
    /// This program will retry the final push of `main` exactly once,
    /// after this interval, in order to ensure that github has the chance
    /// to synchronize itself.
    #[arg(short = 'i', long, default_value_t = 2.5)]
    push_retry_interval: f64,
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
struct StatusCheck {
    #[serde(rename = "__typename")]
    type_name: String,
    name: String,
    status: String,
    conclusion: String,
}

impl StatusCheck {
    fn is_check_run(&self) -> bool {
        self.type_name == "CheckRun"
    }

    fn is_successy(&self) -> bool {
        self.status == "COMPLETED" && (self.conclusion == "SUCCESS" || self.conclusion == "SKIPPED")
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    review_decision: String,
    status_check_rollup: Vec<StatusCheck>,
}

impl Status {
    fn is_approved(&self) -> bool {
        self.review_decision == "APPROVED"
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
        "gh pr view {branch} --json reviewDecision,statusCheckRollup"
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
            .filter(|check| check.is_check_run() && !check.is_successy())
            .map(|check| &check.name)
            .join(", ");
        if !non_success.is_empty() {
            bail!("some ci checks are incomplete or unsuccessful: {non_success}");
        }
    }

    // ensure that the branch is at the tip of origin/main for a linear history
    cmd!(sh, "git fetch").run().context("git fetch")?;
    cmd!(sh, "git checkout {branch}")
        .run()
        .context("git checkout branch")?;
    let rebase_result = cmd!(sh, "git rebase origin/main").run();
    if rebase_result.is_err() {
        cmd!(sh, "git rebase --abort")
            .run()
            .context("aborting rebase")?;
        bail!("{branch} did not cleanly rebase onto origin/main; do so manually and try again");
    }

    // if rebase moved the tip then force-push to ensure github is tracking the new history
    // this resets CI, but doesn't mess with the approvals. We can assume CI is OK, at this point
    let branch_sha = cmd!(sh, "git rev-parse {branch}")
        .read()
        .context("reading branch sha")?;
    let remote_branch_sha = cmd!(sh, "git rev-parse origin/{branch}")
        .read()
        .context("reading remote branch sha")?;
    if branch_sha != remote_branch_sha {
        cmd!(sh, "git push -f")
            .run()
            .context("force-pushing branch")?;
    }

    // we can now actually merge this to main without breaking anything
    cmd!(sh, "git checkout main")
        .run()
        .context("checking out main")?;
    cmd!(sh, "git merge {branch} --ff-only")
        .run()
        .context("performing ff-only merge to main")?;

    // in principle we can now just push; github has some magic to ensure that if you are pushing main
    // to a commit which is at the tip of an approved pr, then it counts it as a manual merge operation
    // and is permitted.
    //
    // sometimes it takes a few seconds for github to catch up, so in the event of a failure we try again
    // a bit later.
    let push_result = cmd!(sh, "git push").run();
    if push_result.is_err() {
        println!("this is normal; retrying in {}s", args.push_retry_interval);
        std::thread::sleep(std::time::Duration::from_secs_f64(args.push_retry_interval));
        cmd!(sh, "git push")
            .run()
            .context("2nd attempt to push to main")?;
    }

    Ok(())
}
