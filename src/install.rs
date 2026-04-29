use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use crate::cli::InstallArgs;
use crate::paths;

pub fn install(args: InstallArgs) -> Result<()> {
    let bin_dir = args
        .bin_dir
        .unwrap_or(paths::home_dir()?.join(".local/bin"));
    fs::create_dir_all(&bin_dir).with_context(|| format!("create {}", bin_dir.display()))?;

    let source = std::env::current_exe().context("resolve current executable")?;
    let dest = bin_dir.join("cx");
    if same_file(&source, &dest) {
        println!("already installed: {}", dest.display());
        return Ok(());
    }
    if dest.exists() && !args.force {
        anyhow::bail!(
            "{} already exists; pass --force to replace it",
            dest.display()
        );
    }

    fs::copy(&source, &dest)
        .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
    make_executable(&dest)?;
    println!("installed: {}", dest.display());
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> bool {
    let Ok(left) = fs::canonicalize(left) else {
        return false;
    };
    let Ok(right) = fs::canonicalize(right) else {
        return false;
    };
    left == right
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("chmod +x {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
