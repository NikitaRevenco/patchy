use std::{
    fs::{self, File},
    io::Write,
};

use crate::{
    commands::help,
    fail,
    flags::{extract_value_from_flag, is_valid_flag, Flag},
    git_commands::{is_valid_branch_name, GIT, GIT_ROOT},
    success,
    types::CommandArgs,
    utils::normalize_commit_msg,
};
use crate::{CONFIG_ROOT, INDENT};
use colored::Colorize;

use super::help::{HELP_FLAG, VERSION_FLAG};

pub static GEN_PATCH_NAME_FLAG: Flag<'static> = Flag {
    short: "-n=",
    long: "--patch-filename=",
    description: "Choose filename for the patch",
};

pub static GEN_PATCH_FLAGS: &[&Flag<'static>; 3] =
    &[&GEN_PATCH_NAME_FLAG, &HELP_FLAG, &VERSION_FLAG];

pub fn gen_patch(args: &CommandArgs) -> anyhow::Result<()> {
    let mut args = args.iter().peekable();
    let mut commit_hashes_with_maybe_custom_patch_filenames = vec![];

    let config_path = GIT_ROOT.join(CONFIG_ROOT);

    let mut no_more_flags = false;

    // TODO: refactor arg iterating logic into a separate function
    // This is duplicated in pr_fetch
    while let Some(arg) = args.next() {
        // After "--", each argument is interpreted literally. This way, we can e.g. use filenames that are named exactly the same as flags
        if arg == "--" {
            no_more_flags = true;
            continue;
        };

        if arg.starts_with('-') && !no_more_flags {
            if !is_valid_flag(arg, GEN_PATCH_FLAGS) {
                fail!("Invalid flag: {arg}");
                let _ = help(Some("gen-patch"));
                std::process::exit(1);
            }

            // Do not consider flags as arguments
            continue;
        }

        let next_arg = args.peek();
        let maybe_custom_patch_filename: Option<String> = next_arg.and_then(|next_arg| {
            extract_value_from_flag(next_arg, &GEN_PATCH_NAME_FLAG)
                .filter(|branch_name| is_valid_branch_name(branch_name))
        });

        if maybe_custom_patch_filename.is_some() {
            args.next();
        };

        commit_hashes_with_maybe_custom_patch_filenames.push((arg, maybe_custom_patch_filename));
    }

    if !config_path.exists() {
        success!(
            "Config directory {} does not exist, creating it...",
            config_path.to_string_lossy()
        );
        fs::create_dir(&config_path)?;
    }

    for (patch_commit_hash, maybe_custom_patch_name) in
        commit_hashes_with_maybe_custom_patch_filenames
    {
        let Ok(patch_contents) = GIT(&[
            "diff",
            &format!("{}^", patch_commit_hash),
            patch_commit_hash,
        ]) else {
            fail!("Could not get patch output for patch {}", patch_commit_hash);
            continue;
        };

        // 1. if the user provides a custom filename for the patch file, use that
        // 2. otherwise use the commit message
        // 3. if all fails use the commit hash
        let patch_filename = maybe_custom_patch_name.unwrap_or({
            GIT(&["log", "--format=%B", "--max-count=1", patch_commit_hash])
                .map(|commit_msg| normalize_commit_msg(&commit_msg))
                .unwrap_or(patch_commit_hash.to_string())
        });

        let patch_filename = format!("{patch_filename}.patch");

        let patch_file_path = config_path.join(&patch_filename);

        let mut file = File::create(&patch_file_path)?;

        file.write_all(patch_contents.as_bytes())?;

        success!(
            "Created patch file at {}",
            patch_file_path.to_string_lossy()
        )
    }

    Ok(())
}
