use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;

use crate::cli::ProtocolExportArgs;
use crate::run;

pub fn export(args: ProtocolExportArgs) -> Result<()> {
    let real_codex = run::resolve_codex_bin(args.codex_bin.as_deref())?;
    let formats = ExportFormats::from_args(args.json_schema, args.typescript);
    fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;

    if formats.json_schema {
        run_generator(
            &real_codex,
            "generate-json-schema",
            &args.out.join("json-schema"),
            args.experimental,
        )?;
    }
    if formats.typescript {
        run_generator(
            &real_codex,
            "generate-ts",
            &args.out.join("typescript"),
            args.experimental,
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExportFormats {
    json_schema: bool,
    typescript: bool,
}

impl ExportFormats {
    fn from_args(json_schema: bool, typescript: bool) -> Self {
        if !json_schema && !typescript {
            return Self {
                json_schema: true,
                typescript: true,
            };
        }
        Self {
            json_schema,
            typescript,
        }
    }
}

fn run_generator(
    real_codex: &Path,
    generator: &str,
    out_dir: &Path,
    experimental: bool,
) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;
    let mut command = Command::new(real_codex);
    command
        .arg("app-server")
        .arg(generator)
        .arg("--out")
        .arg(out_dir);
    if experimental {
        command.arg("--experimental");
    }
    let status = command
        .status()
        .with_context(|| format!("run codex app-server {generator}"))?;
    if !status.success() {
        anyhow::bail!("codex app-server {generator} exited with {status}");
    }
    println!("wrote {generator}: {}", out_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_formats_default_to_both() {
        assert_eq!(
            ExportFormats::from_args(false, false),
            ExportFormats {
                json_schema: true,
                typescript: true,
            }
        );
    }

    #[test]
    fn export_formats_respect_explicit_subset() {
        assert_eq!(
            ExportFormats::from_args(true, false),
            ExportFormats {
                json_schema: true,
                typescript: false,
            }
        );
        assert_eq!(
            ExportFormats::from_args(false, true),
            ExportFormats {
                json_schema: false,
                typescript: true,
            }
        );
    }
}
