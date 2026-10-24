use std::ffi::OsStr;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Runs a command to completion; its output is the error when it fails.
pub async fn run<I, S>(program: impl AsRef<OsStr>, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    output(program, args).await.map(drop)
}

/// Runs a command to completion and returns its stdout; its output is the error when it fails.
pub async fn output<I, S>(program: impl AsRef<OsStr>, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args);
    let command_line = describe(&command);
    let finished = command
        .output()
        .await
        .with_context(|| format!("running {command_line}"))?;
    if !finished.status.success() {
        let stderr = String::from_utf8_lossy(&finished.stderr);
        let stdout = String::from_utf8_lossy(&finished.stdout);
        let complaint = [stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        bail!("{command_line}: {}: {complaint}", finished.status);
    }
    Ok(String::from_utf8_lossy(&finished.stdout).into_owned())
}

/// Whether a command runs and succeeds, its output aside.
pub async fn succeeds<I, S>(program: impl AsRef<OsStr>, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .args(args)
        .output()
        .await
        .is_ok_and(|finished| finished.status.success())
}

pub fn describe(command: &Command) -> String {
    let std_command = command.as_std();
    std::iter::once(std_command.get_program())
        .chain(std_command.get_args())
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}
