use std::fs;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn temp_root(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from("/tmp").join(format!("cx-{name}-{}-{unique}", std::process::id()))
}

fn cx_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cx")
}

fn shell_quote(path: &Path) -> String {
    let text = path.display().to_string();
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn write_fake_codex(root: &Path, log: &Path) -> PathBuf {
    let script = root.join("fake-codex.sh");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
LOG={}
printf '%s|%s\n' "$CODEX_HOME" "$*" >> "$LOG"
trap 'exit 0' INT TERM
while IFS= read -r line; do
  printf 'fake:%s\n' "$line"
done
"#,
            shell_quote(log)
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

fn write_rate_limit_fake_codex(root: &Path, log: &Path) -> PathBuf {
    let script = root.join("fake-codex-rate-limit.sh");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
LOG={}
printf '%s|%s\n' "$CODEX_HOME" "$*" >> "$LOG"
case "$CODEX_HOME" in
  *bus1/home) printf 'HTTP 429 too many requests\n' ;;
esac
trap 'exit 0' INT TERM
while IFS= read -r line; do
  printf 'fake:%s\n' "$line"
done
"#,
            shell_quote(log)
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

fn setup_manager(root: &Path) -> PathBuf {
    let manager = root.join("profile-manager");
    for slot in ["bus1", "bus2"] {
        fs::create_dir_all(manager.join("slots").join(slot).join("home")).unwrap();
    }
    fs::write(manager.join("rotation.txt"), "bus1\nbus2\n").unwrap();
    manager
}

fn write_api_key_auth(manager: &Path) {
    for slot in ["bus1", "bus2"] {
        fs::write(
            manager
                .join("slots")
                .join(slot)
                .join("home")
                .join("auth.json"),
            r#"{"OPENAI_API_KEY":"test-key"}"#,
        )
        .unwrap();
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_log(log: &Path) -> String {
    fs::read_to_string(log).unwrap_or_default()
}

fn terminate_supervisor(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => return,
            Err(err) => panic!("wait supervisor: {err}"),
        }
    }
}

#[test]
fn managed_supervisor_status_rotate_and_resume_control_path() {
    let root = temp_root("managed-cli");
    fs::create_dir_all(&root).unwrap();
    let manager = setup_manager(&root);
    let log = root.join("fake.log");
    let fake_codex = write_fake_codex(&root, &log);
    let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut supervisor = Command::new(cx_bin())
        .current_dir(cwd)
        .arg("--managed")
        .arg("--manager-dir")
        .arg(&manager)
        .arg("--codex-bin")
        .arg(&fake_codex)
        .arg("--slot")
        .arg("bus1")
        .arg("--cx-quiet")
        .arg("resume")
        .arg("sid-managed-1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !wait_for(|| read_log(&log).contains("bus1/home|resume sid-managed-1")) {
            let status = supervisor.try_wait().unwrap();
            let mut stderr = String::new();
            if status.is_some() {
                if let Some(mut pipe) = supervisor.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
            }
            panic!(
                "initial fake codex did not start; status: {:?}; log: {:?}; stderr: {:?}",
                status,
                read_log(&log),
                stderr
            );
        }

        let status = Command::new(cx_bin())
            .current_dir(cwd)
            .arg("managed")
            .arg("status")
            .arg("--manager-dir")
            .arg(&manager)
            .output()
            .unwrap();
        assert!(status.status.success());
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(status_text.contains("slot: bus1"));
        assert!(status_text.contains("session: sid-managed-1"));

        let rotate = Command::new(cx_bin())
            .current_dir(cwd)
            .arg("managed")
            .arg("rotate")
            .arg("--manager-dir")
            .arg(&manager)
            .arg("--slot")
            .arg("bus2")
            .output()
            .unwrap();
        assert!(rotate.status.success());
        assert!(
            wait_for(|| read_log(&log).contains("bus2/home|resume sid-managed-1")),
            "rotate did not restart fake codex; log: {:?}",
            read_log(&log)
        );

        let resume = Command::new(cx_bin())
            .current_dir(cwd)
            .arg("managed")
            .arg("resume")
            .arg("--manager-dir")
            .arg(&manager)
            .arg("sid-managed-2")
            .arg("--slot")
            .arg("bus1")
            .arg("--continue")
            .output()
            .unwrap();
        assert!(resume.status.success());
        assert!(
            wait_for(|| read_log(&log).contains("bus1/home|resume sid-managed-2 继续")),
            "resume did not restart fake codex with continue; log: {:?}",
            read_log(&log)
        );
    }));

    terminate_supervisor(&mut supervisor);
    let _ = fs::remove_dir_all(&root);
    result.unwrap();
}

#[test]
fn managed_supervisor_auto_rotates_api_key_slot_on_provider_limit() {
    let root = temp_root("managed-auto-rotate");
    fs::create_dir_all(&root).unwrap();
    let manager = setup_manager(&root);
    write_api_key_auth(&manager);
    let log = root.join("fake.log");
    let fake_codex = write_rate_limit_fake_codex(&root, &log);
    let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut supervisor = Command::new(cx_bin())
        .current_dir(cwd)
        .arg("--managed")
        .arg("--manager-dir")
        .arg(&manager)
        .arg("--codex-bin")
        .arg(&fake_codex)
        .arg("--slot")
        .arg("bus1")
        .arg("--cx-quiet")
        .arg("resume")
        .arg("sid-auto-rotate")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            wait_for(|| read_log(&log).contains("bus1/home|resume sid-auto-rotate")),
            "initial fake codex did not start; log: {:?}",
            read_log(&log)
        );
        assert!(
            wait_for(|| read_log(&log).contains("bus2/home|resume sid-auto-rotate")),
            "automatic rotate did not start next slot; log: {:?}",
            read_log(&log)
        );

        let status = Command::new(cx_bin())
            .current_dir(cwd)
            .arg("managed")
            .arg("status")
            .arg("--manager-dir")
            .arg(&manager)
            .output()
            .unwrap();
        assert!(status.status.success());
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(status_text.contains("slot: bus2"));
        assert!(status_text.contains("session: sid-auto-rotate"));
    }));

    terminate_supervisor(&mut supervisor);
    let _ = fs::remove_dir_all(&root);
    result.unwrap();
}
