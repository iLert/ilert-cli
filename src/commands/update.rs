//! `ilert update` — replace this binary by running the installer it shipped
//! with.
//!
//! The installer is `install.sh` from this repository, compiled in with
//! [`include_bytes!`]. It already resolves the right asset for this platform,
//! verifies the published SHA256 checksum as a hard gate, checks GitHub's
//! release attestation where `gh` can run, and escalates to sudo only with the
//! exact command printed first. Reimplementing that here would mean keeping a
//! second copy of those trust rules in step with the first.
//!
//! Embedded rather than downloaded, and that is the whole point: a script
//! fetched from a branch is mutable and unsigned, so a compromised or merely
//! mistaken `master` could serve an installer with the checksum and attestation
//! checks removed — the update path would then be weaker than the install path
//! it exists to repeat. These bytes travelled inside the release binary the
//! user already verified, and they cannot change between then and now. The
//! trade is that an old binary carries an old installer; that is acceptable
//! because the installer's job is to fetch the *latest release*, which it does
//! the same way in every version, and a change to the installer itself reaches
//! people on their next update.
//!
//! Replacing the running binary is safe on the platforms this supports:
//! `install_binary` swaps the file with a rename, which leaves the already
//! mapped inode alone until this process exits.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::Result;
use clap::{ArgMatches, Command};
use colored::Colorize;

use crate::cli::RunContext;
use crate::config::ConfigManager;
use crate::errors::CliError;

/// The installer, as it stood when this binary was built.
const INSTALLER: &[u8] = include_bytes!("../../install.sh");

pub fn command() -> Command {
    Command::new("update").about("Update the CLI to the latest release")
}

pub async fn handle(_matches: &ArgMatches, ctx: &RunContext) -> Result<()> {
    if cfg!(windows) {
        return Err(CliError::user(
            "`ilert update` runs the POSIX installer, which supports macOS and Linux only. \
             Download the release binary manually from \
             https://github.com/iLert/ilert-cli/releases/latest.",
        )
        .into());
    }

    // Which file this replaces, decided here rather than by the installer's own
    // guess. Without it the installer picks a conventional location, which on a
    // machine with more than one ilert can be a different file than the one
    // running — and then a successful-looking update leaves the caller on the
    // old binary.
    let target = std::env::current_exe().map_err(|e| {
        CliError::user(format!(
            "Could not determine which ilert binary is running, so there is nothing safe to \
             replace: {e}"
        ))
    })?;

    // Refuse a package-managed install we can recognise. Writing over one
    // leaves the manager believing it installed the version it recorded, and
    // its next upgrade silently reverts ours — and the installer would likely
    // not even land on the same path, leaving two copies and no clear winner.
    // This is not a confirmation, so `--yes` does not answer it.
    //
    // Recognition is by install path, which covers the managers that install
    // somewhere distinctive and cannot cover the ones that do not: an apt or
    // rpm package puts its binary at /usr/bin/ilert, exactly where the
    // installer would. Those are updated in place, and the README says so
    // rather than promising a guarantee this cannot make.
    if let Some(manager) = owning_package_manager(Some(&target)) {
        return Err(CliError::user(format!(
            "This ilert binary was installed by {} ({}).\n\
             Update it with `{}`, so the two stay in agreement.",
            manager.name,
            target.display(),
            manager.upgrade_hint
        ))
        .into());
    }

    consent(ctx, &target)?;

    let script_path = stage_installer()?;
    let outcome = run_installer(&script_path, &target);
    // The staged copy goes whether or not the installer succeeded — it is an
    // executable file in a predictable place, and nothing reads it again.
    let _ = std::fs::remove_file(&script_path);
    outcome?;

    // The update check caches "latest vs current" for twelve hours, and both
    // halves of that comparison just changed. Dropping the file makes the next
    // run re-check rather than repeat a notice about the version we now are.
    if let Ok(path) = crate::commands::version::check_file_path() {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}

/// Ask before replacing the binary that is running.
///
/// A caller that cannot be asked is refused rather than prompted, for the same
/// reason destructive API calls are: a prompt nobody can answer is a hang.
fn consent(ctx: &RunContext, target: &Path) -> Result<()> {
    if ctx.auto_confirm {
        return Ok(());
    }

    if !ctx.can_prompt() {
        return Err(CliError::user(format!(
            "`ilert update` downloads the latest release and replaces {}. \
             Re-run with --yes to confirm.",
            target.display()
        ))
        .into());
    }

    eprintln!(
        "{} downloads the latest release, verifies its checksum, and replaces {}.",
        "ilert update".bold(),
        target.display()
    );
    let confirmed = dialoguer::Confirm::new()
        .with_prompt("Continue?")
        .default(true)
        .interact()?;

    if confirmed {
        Ok(())
    } else {
        Err(CliError::Cancelled.into())
    }
}

/// Write the embedded installer where only this user can read or run it.
///
/// Created with owner-only permissions in one step rather than written and then
/// chmodded, so there is no window in which another user on the machine could
/// open — or replace — the script between the write and the `bash` that runs
/// it. The PID in the name keeps two concurrent updates from sharing a file,
/// and `create_new` refuses to reuse one left behind.
fn stage_installer() -> Result<PathBuf> {
    let dir = ConfigManager::cache_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("install-{}.sh", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }

    let mut file = options.open(&path)?;
    file.write_all(INSTALLER)?;
    file.sync_all()?;

    Ok(path)
}

/// Run the staged installer against `target`, inheriting this process's streams.
///
/// Inherited stdio is the point: `install.sh` prints its progress, and its sudo
/// prompt reads from the terminal. Capturing the output would turn the one step
/// that asks for a password into a silent hang.
fn run_installer(script_path: &Path, target: &Path) -> Result<()> {
    let status = Process::new("bash")
        // `--` first, so nothing about the path can be read as an option.
        .arg("--")
        .arg(script_path)
        // The exact file to replace — and the one the installer runs at the end
        // to prove the new bytes work.
        .env("ILERT_INSTALL_URI", target)
        // Tells install.sh it is finishing an update rather than a first
        // install, so it ends with the version instead of the full help.
        .env("ILERT_UPDATE", "1")
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::user(
                    "`bash` was not found on PATH, and the installer needs it. \
                     Install bash, or download the release binary manually from \
                     https://github.com/iLert/ilert-cli/releases/latest.",
                )
            } else {
                CliError::user(format!("Could not run the installer: {e}"))
            }
        })?;

    if status.success() {
        return Ok(());
    }

    // Deliberately not "ilert was not updated". The installer can fail at any
    // of its steps, and the later ones run after the binary has already been
    // replaced — claiming nothing changed would be a guess, and the wrong guess
    // sends someone off to debug a version they are no longer running. The
    // installer has already explained *what* failed on the inherited stderr;
    // this adds only what this process can actually vouch for.
    let detail = match status.code() {
        Some(code) => format!("The installer exited with status {code}"),
        None => "The installer was terminated by a signal".to_string(),
    };
    Err(CliError::user(format!(
        "{detail}; the update did not complete. The binary at {} may or may not have been \
         replaced — run `{} version` to see which one is in place.",
        target.display(),
        target.display()
    ))
    .into())
}

struct PackageManager {
    name: &'static str,
    upgrade_hint: &'static str,
}

/// Recognise an install another tool owns, from where the binary sits.
///
/// Path shape is the only signal available to a binary that was handed no
/// provenance, so this is a heuristic — but it errs toward refusing, and a
/// refusal names a working alternative rather than a dead end.
fn owning_package_manager(exe: Option<&Path>) -> Option<PackageManager> {
    let path = exe?.to_string_lossy().replace('\\', "/");

    // Matched on path segments so a user directory that merely contains one of
    // these words — /home/cellar/bin/ilert — is not mistaken for the manager.
    let has = |segment: &str| path.contains(&format!("/{segment}/"));

    if has("Cellar") || has("homebrew") || has("linuxbrew") {
        return Some(PackageManager {
            name: "Homebrew",
            upgrade_hint: "brew upgrade ilert",
        });
    }
    if path.contains("/nix/store/") {
        return Some(PackageManager {
            name: "Nix",
            upgrade_hint: "nix profile upgrade ilert",
        });
    }
    if has("snap") {
        return Some(PackageManager {
            name: "snap",
            upgrade_hint: "snap refresh ilert",
        });
    }
    if has(".cargo") && has("bin") {
        return Some(PackageManager {
            name: "cargo",
            upgrade_hint: "cargo install ilert --force",
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer is compiled in, so it has to be the real one — a stub or
    /// an empty file would fail only at the moment someone runs an update.
    #[test]
    fn the_embedded_installer_is_the_real_one() {
        let text = std::str::from_utf8(INSTALLER).expect("installer is not UTF-8");
        assert!(text.starts_with("#!"), "installer has no shebang");
        // The steps an update depends on. If any of these is ever renamed, this
        // fails here rather than on a user's machine mid-update.
        for marker in [
            "verify_checksum",
            "verify_attestation",
            "install_binary",
            "resolve_install_target",
            "ILERT_INSTALL_URI",
            "ILERT_UPDATE",
        ] {
            assert!(text.contains(marker), "installer is missing {marker}");
        }
    }

    #[test]
    fn a_package_managed_binary_is_recognised() {
        for (exe, manager) in [
            ("/opt/homebrew/bin/ilert", "Homebrew"),
            ("/usr/local/Cellar/ilert/1.2.0/bin/ilert", "Homebrew"),
            ("/home/linuxbrew/.linuxbrew/bin/ilert", "Homebrew"),
            ("/nix/store/abc123-ilert-1.2.0/bin/ilert", "Nix"),
            ("/snap/ilert/current/bin/ilert", "snap"),
            ("/home/me/.cargo/bin/ilert", "cargo"),
        ] {
            let found = owning_package_manager(Some(Path::new(exe)));
            assert_eq!(
                found.map(|m| m.name),
                Some(manager),
                "{exe} should be attributed to {manager}"
            );
        }
    }

    /// The installer's own destinations must stay updatable — attributing one
    /// of them to a package manager would refuse the ordinary case outright,
    /// and there is no flag to override it.
    #[test]
    fn an_installer_managed_binary_is_left_alone() {
        for exe in [
            "/usr/local/bin/ilert",
            "/usr/bin/ilert",
            "/home/me/.local/bin/ilert",
            // A directory that merely contains a manager's name inside a longer
            // word is not that manager.
            "/home/cellars/bin/ilert",
            "/opt/snapshots/bin/ilert",
            "/home/me/nix-backups/bin/ilert",
        ] {
            assert!(
                owning_package_manager(Some(Path::new(exe))).is_none(),
                "{exe} should be treated as an installer-managed binary"
            );
        }
    }

    #[test]
    fn an_unknown_location_is_not_attributed() {
        assert!(owning_package_manager(None).is_none());
    }
}
