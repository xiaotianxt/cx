use std::ffi::OsString;
use std::io;
use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;

use anyhow::Context;
use anyhow::Result;
use serde_json::Map;
use serde_json::Value;

// Desktop asks app-server for the active provider by sending a null/omitted
// modelProviders filter. CX keeps provider metadata intact and changes only
// that presentation-layer default while proxying the bundled CLI over stdio.
pub(crate) const ENABLE_ENV: &str = "CX_DESKTOP_CODEX_PROXY";
pub(crate) const REAL_CODEX_ENV: &str = "CX_DESKTOP_REAL_CODEX_CLI";
pub(crate) const CODEX_CLI_PATH_ENV: &str = "CODEX_CLI_PATH";
pub(crate) const FORCE_CLI_ENV: &str = "CODEX_APP_SERVER_FORCE_CLI";

pub(crate) fn enabled() -> bool {
    std::env::var_os(ENABLE_ENV).is_some_and(|value| value == "1")
}

pub(crate) fn run(args: Vec<OsString>) -> Result<()> {
    let real_codex = std::env::var_os(REAL_CODEX_ENV)
        .map(PathBuf::from)
        .context("missing CX desktop real Codex CLI path")?;
    if !real_codex.is_file() {
        anyhow::bail!(
            "CX desktop real Codex CLI does not exist: {}",
            real_codex.display()
        );
    }

    if !is_app_server_command(&args) {
        return run_passthrough(real_codex, args);
    }

    run_app_server_proxy(real_codex, args)
}

fn is_app_server_command(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "app-server")
}

fn base_command(real_codex: PathBuf, args: Vec<OsString>) -> Command {
    let mut command = Command::new(&real_codex);
    command
        .env_remove(ENABLE_ENV)
        .env_remove(REAL_CODEX_ENV)
        .env(CODEX_CLI_PATH_ENV, &real_codex)
        .args(args);
    command
}

fn run_passthrough(real_codex: PathBuf, args: Vec<OsString>) -> Result<()> {
    let status = base_command(real_codex, args)
        .status()
        .context("run bundled Codex CLI")?;
    if !status.success() {
        anyhow::bail!("bundled Codex CLI exited with {status}");
    }
    Ok(())
}

fn run_app_server_proxy(real_codex: PathBuf, args: Vec<OsString>) -> Result<()> {
    let mut child = base_command(real_codex, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start bundled Codex app-server")?;
    let child_stdin = child.stdin.take().context("open Codex app-server stdin")?;
    let child_stdout = child
        .stdout
        .take()
        .context("open Codex app-server stdout")?;
    let _input_thread = thread::spawn(move || {
        let stdin = io::stdin();
        if let Err(error) = forward_requests(stdin.lock(), child_stdin) {
            eprintln!("cx desktop request proxy stopped: {error}");
        }
    });
    let output_thread = thread::spawn(move || {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        let mut child_stdout = child_stdout;
        io::copy(&mut child_stdout, &mut stdout).map(|_| ())
    });

    let status = child.wait().context("wait for bundled Codex app-server")?;
    output_thread
        .join()
        .map_err(|_| anyhow::anyhow!("CX desktop response proxy panicked"))?
        .context("forward Codex app-server responses")?;
    if !status.success() {
        anyhow::bail!("bundled Codex app-server exited with {status}");
    }
    Ok(())
}

fn forward_requests<R: BufRead, W: Write>(mut reader: R, mut writer: W) -> io::Result<()> {
    forward_lines(&mut reader, &mut writer, rewrite_request_line)
}

fn forward_lines<R: BufRead, W: Write, F>(
    reader: &mut R,
    writer: &mut W,
    mut transform: F,
) -> io::Result<()>
where
    F: FnMut(&[u8]) -> Option<Vec<u8>>,
{
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if let Some(rewritten) = transform(&line) {
            writer.write_all(&rewritten)?;
        } else {
            writer.write_all(&line)?;
        }
        writer.flush()?;
    }
    Ok(())
}

fn rewrite_request_line(line: &[u8]) -> Option<Vec<u8>> {
    let had_newline = line.ends_with(b"\n");
    let json_bytes = line.strip_suffix(b"\n").unwrap_or(line);
    let mut request: Value = serde_json::from_slice(json_bytes).ok()?;
    if request.get("method").and_then(Value::as_str) != Some("thread/list") {
        return None;
    }

    let params = request.get_mut("params")?.as_object_mut()?;
    if !requests_all_providers(params) {
        return None;
    }

    params.insert("modelProviders".to_string(), Value::Array(Vec::new()));

    let mut rewritten = serde_json::to_vec(&request).ok()?;
    if had_newline {
        rewritten.push(b'\n');
    }
    Some(rewritten)
}

fn requests_all_providers(params: &Map<String, Value>) -> bool {
    match params.get("modelProviders") {
        None | Some(Value::Null) => true,
        Some(Value::Array(providers)) => providers.is_empty(),
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rewrite(value: Value) -> Value {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        let rewritten = rewrite_request_line(&line).expect("request should be rewritten");
        serde_json::from_slice(&rewritten).unwrap()
    }

    #[test]
    fn default_thread_list_reads_all_providers_without_changing_db_mode() {
        let rewritten = rewrite(json!({
            "id": 7,
            "method": "thread/list",
            "params": {
                "cursor": null,
                "limit": 100,
                "modelProviders": null,
                "sourceKinds": ["cli", "appServer"],
                "useStateDbOnly": true
            }
        }));

        assert_eq!(rewritten["params"]["modelProviders"], json!([]));
        assert_eq!(rewritten["params"]["useStateDbOnly"], true);
    }

    #[test]
    fn explicit_provider_filter_is_preserved() {
        let mut line = serde_json::to_vec(&json!({
            "id": 1,
            "method": "thread/list",
            "params": {
                "modelProviders": ["pku"],
                "useStateDbOnly": true
            }
        }))
        .unwrap();
        line.push(b'\n');

        assert!(rewrite_request_line(&line).is_none());
    }

    #[test]
    fn detects_app_server_subcommand_after_config_flags() {
        assert!(is_app_server_command(&[
            OsString::from("-c"),
            OsString::from("features.code_mode_host=true"),
            OsString::from("app-server"),
        ]));
        assert!(!is_app_server_command(&[OsString::from("--version")]));
    }
}
