use std::fs;

use anyhow::{anyhow, Context};
use colored::Colorize;
use dialoguer::Confirm;

use crate::{
    backup::{backup_files, restore_backup},
    commands::{init, pr_fetch::ignore_octothorpe},
    confirm_prompt, fail,
    flags::{Flag, IS_VERBOSE},
    git_commands::{
        add_remote_branch, checkout_from_remote, clean_up_remote, fetch_pull_request,
        merge_pull_request, GIT, GIT_ROOT,
    },
    info, success, trace,
    types::{Branch, BranchAndRemote, CommandArgs, Configuration, Remote},
    utils::{display_link, with_uuid},
    APP_NAME, CONFIG_FILE, CONFIG_ROOT, INDENT,
};

pub static RUN_YES_FLAG: Flag<'static> = Flag {
    short: "-y",
    long: "--yes",
    description: "Do not prompt when overwriting local-branch specified in the config",
};

/// Parses user inputs of the form "(<anything>)+ @ <commit-hash>"
///
/// Returns the user's input but also the commit hash if it exists
pub fn parse_if_maybe_hash(input: &str, syntax: &str) -> (String, Option<String>) {
    let parts: Vec<_> = input.split(syntax).collect();

    let len = parts.len();

    if len == 1 {
        // The string does not contain any " @ ", so the user chose to use the latest commit rather than a specific one
        (input.into(), None)
    } else {
        // They want to use a specific commit
        let output: String = parts[0..len - 1].iter().map(|s| String::from(*s)).collect();
        let commit_hash: Option<String> = Some(parts[len - 1].into());
        (output, commit_hash)
    }
}

pub async fn run(args: &CommandArgs) -> anyhow::Result<()> {
    println!();

    let config_path = GIT_ROOT.join(CONFIG_ROOT);
    let has_yes_flag = RUN_YES_FLAG.is_in_args(args);

    let config_file_path = config_path.join(CONFIG_FILE);

    let Ok(config_raw) = fs::read_to_string(config_file_path.clone()) else {
        fail!("Could not find configuration file at {CONFIG_ROOT}/{CONFIG_FILE}");

        // We don't want to have *any* sort of prompt when using the -y flag since that would be problematic in scripts
        if !has_yes_flag
            && confirm_prompt!(
                "Would you like us to run {} {} to initialize it?",
                "patchy".bright_blue(),
                "init".bright_yellow(),
            )
        {
            if let Err(err) = init(args) {
                fail!("{err}");
                std::process::exit(1);
            };
        } else if has_yes_flag {
            eprintln!(
                "You can create it with {} {}",
                "patchy".bright_blue(),
                "init".bright_yellow()
            )
        }

        // We don't want to read the default configuration file as config_raw. Since it's empty there's no reason why the user would want to run it.

        std::process::exit(0);
    };

    trace!("Using configuration file {config_file_path:?}");

    let config = toml::from_str::<Configuration>(&config_raw).context(format!(
        "Could not parse `{CONFIG_ROOT}/{CONFIG_FILE}` configuration file"
    ))?;

    let (remote_branch, commit_hash) = parse_if_maybe_hash(&config.remote_branch, " @ ");

    if config.repo.is_empty() {
        return Err(anyhow::anyhow!(
            r#"You haven't specified a `repo` in your config, which can be for example:
  - "helix-editor/helix"
  - "microsoft/vscode"

  For more information see this guide: https://github.com/NikitaRevenco/patchy/blob/main/README.md""#
        ));
    }

    let config_files = fs::read_dir(&config_path).context(format!(
        "Could not read files in directory {:?}",
        &config_path
    ))?;

    let backed_up_files = backup_files(config_files)
        .context("Could not create backups for configuration files, aborting.")?;

    let info = BranchAndRemote {
        branch: Branch {
            upstream_branch_name: remote_branch.clone(),
            local_branch_name: with_uuid(&remote_branch),
        },
        remote: Remote {
            repository_url: format!("https://github.com/{}.git", config.repo),
            local_remote_alias: with_uuid(&config.repo),
        },
    };

    add_remote_branch(&info, &commit_hash)?;

    let previous_branch = checkout_from_remote(
        &info.branch.local_branch_name,
        &info.remote.local_remote_alias,
    )?;

    let client = reqwest::Client::new();

    if config.pull_requests.is_empty() {
        info!(
            "You haven't specified any pull requests to fetch in your config, {}",
            display_link(
                "see the instructions on how to configure patchy.",
                "https://github.com/NikitaRevenco/patchy?tab=readme-ov-file#config"
            )
        )
    } else {
        // TODO: make this concurrent, see https://users.rust-lang.org/t/processing-subprocesses-concurrently/79638/3
        // Git cannot handle multiple threads executing commands in the same repository, so we can't use threads, but we can run processes in the background
        for pull_request in config.pull_requests.iter() {
            let pull_request = ignore_octothorpe(pull_request);
            let (pull_request, commit_hash) = parse_if_maybe_hash(&pull_request, " @ ");
            // TODO: refactor this to not use such deep nesting
            match fetch_pull_request(&config.repo, &pull_request, &client, None, &commit_hash).await
            {
                Ok((response, info)) => {
                    match merge_pull_request(
                        info,
                        &pull_request,
                        &response.title,
                        &response.html_url,
                    )
                    .await
                    {
                        Ok(()) => {
                            success!(
                                "Merged pull request {}",
                                display_link(
                                    &format!(
                                        "{}{}{}{}",
                                        "#".bright_blue(),
                                        pull_request.bright_blue(),
                                        " ".bright_blue(),
                                        &response.title.bright_blue().italic()
                                    ),
                                    &response.html_url
                                ),
                            )
                        }
                        Err(err) => {
                            fail!("{err}");
                            continue;
                        }
                    };
                }
                Err(err) => {
                    fail!("Could not fetch branch from remote\n{err}");
                    continue;
                }
            }
        }
    }

    if let Err(err) = fs::create_dir(GIT_ROOT.join(CONFIG_ROOT)) {
        GIT(&["checkout", &previous_branch])?;

        clean_up_remote(
            &info.remote.local_remote_alias,
            &info.branch.local_branch_name,
        )?;

        return Err(anyhow!("Could not create directory {CONFIG_ROOT}\n{err}"));
    };

    for (file_name, _file, contents) in backed_up_files.iter() {
        restore_backup(file_name, contents).context("Could not restore backups")?;

        // apply patches if they exist
        if let Some(ref patches) = config.patches {
            let file_name = file_name
                .to_str()
                .and_then(|file_name| file_name.get(0..file_name.len() - 6))
                .unwrap_or_default();

            if patches.contains(file_name) {
                GIT(&[
                    "am",
                    "--keep-cr",
                    "--signoff",
                    &format!(
                        "{}/{file_name}.patch",
                        GIT_ROOT.join(CONFIG_ROOT).to_str().unwrap_or_default()
                    ),
                ])
                .context(format!("Could not apply patch {file_name}, skipping"))?;

                let last_commit_message = GIT(&["log", "-1", "--format=%B"])?;
                success!(
                    "Applied patch {file_name} {}",
                    last_commit_message
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .bright_blue()
                        .italic()
                );
            }
        }
    }

    GIT(&["add", CONFIG_ROOT])?;
    GIT(&[
        "commit",
        "--message",
        &format!("{APP_NAME}: Restore configuration files"),
    ])?;

    let temporary_branch = with_uuid("temp-branch");

    GIT(&["switch", "--create", &temporary_branch])?;

    clean_up_remote(
        &info.remote.local_remote_alias,
        &info.branch.local_branch_name,
    )?;

    if has_yes_flag
        || confirm_prompt!(
            "Overwrite branch {}? This is irreversible.",
            config.local_branch.cyan()
        )
    {
        // forcefully renames the branch we are currently on into the branch specified by the user.
        // WARNING: this is a destructive action which erases the original branch
        GIT(&[
            "branch",
            "--move",
            "--force",
            &temporary_branch,
            &config.local_branch,
        ])?;
        if has_yes_flag {
            info!(
                "Overwrote branch {} since you supplied the {} flag",
                config.local_branch.cyan(),
                "--yes".bright_magenta()
            );
        }
        println!("\n{INDENT}{}", "  Success!\n".bright_green().bold());
    } else {
        let command = format!(
            "  git branch --move --force {temporary_branch} {}",
            config.local_branch
        );
        let command = format!("\n{INDENT}{}\n", command.bright_magenta());
        println!(
            "\n{INDENT}  You can still manually overwrite {} with the following command:\n  {command}",
            config.local_branch.cyan(),
        );
        std::process::exit(1)
    }

    Ok(())
}
