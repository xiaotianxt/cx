use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;

pub fn run_from_args(args: Vec<OsString>) -> Result<()> {
    if io::stdin().is_terminal() {
        return crate::run::run_from_args(args);
    }

    debug("reading piped stdin");
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("read piped stdin")?;
    debug(&format!(
        "finished reading piped stdin: {} bytes",
        input.len()
    ));

    let combined_prompt = build_stdin_prompt(&args, &input);
    debug("starting codex tui");

    if script_command_available() {
        exec_with_script(combined_prompt)
    } else {
        exec_self_with_tty(vec![OsString::from("--"), OsString::from(combined_prompt)])
    }
}

fn build_stdin_prompt(args: &[OsString], input: &str) -> String {
    let prompt = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let prompt = if prompt.is_empty() {
        "Use the following stdin as context."
    } else {
        prompt.as_str()
    };
    format!("{prompt}\n\n<stdin>\n{input}\n</stdin>")
}

fn exec_with_script(combined_prompt: String) -> Result<()> {
    let cx = current_exe()?;
    let tty = File::open("/dev/tty").context("open /dev/tty")?;
    let mut command = Command::new("script");
    command
        .arg("-q")
        .arg("/dev/null")
        .arg(cx)
        .arg("--")
        .arg(combined_prompt)
        .stdin(tty);
    exec(command)
}

fn exec_self_with_tty(args: Vec<OsString>) -> Result<()> {
    let mut command = Command::new(current_exe()?);
    command
        .args(args)
        .stdin(File::open("/dev/tty").context("open /dev/tty")?);
    exec(command)
}

fn current_exe() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CX_BIN") {
        return Ok(PathBuf::from(path));
    }
    std::env::current_exe().context("resolve current executable")
}

fn script_command_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("script").is_file())
}

fn debug(message: &str) {
    if std::env::var_os("CX_DEBUG").is_none() {
        return;
    }
    eprintln!("cx: {message}");
}

fn exec(mut command: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("exec cx command")
    }
    #[cfg(not(unix))]
    {
        let status = command.status().context("run cx command")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_wraps_stdin() {
        assert_eq!(
            build_stdin_prompt(&[], "hello\n"),
            "Use the following stdin as context.\n\n<stdin>\nhello\n\n</stdin>"
        );
    }

    #[test]
    fn args_become_prompt_when_stdin_is_piped() {
        assert_eq!(
            build_stdin_prompt(
                &[OsString::from("summarize"), OsString::from("briefly")],
                "hello"
            ),
            "summarize briefly\n\n<stdin>\nhello\n</stdin>"
        );
    }
}
