//! End-to-end WinFsp coverage through the `winfsp_run` bin: one mounted
//! Workspace per invocation, `cmd.exe` as the Windows client. Every test is
//! ignored by default: they need the WinFsp driver and a built bin, and they
//! mount real volumes (`cargo test --test winfsp_e2e -- --ignored`).
#![cfg(windows)]
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

/// This machine's WinFsp install keeps its DLL out of System32; every child
/// gets it on PATH.
const WINFSP_BIN: &str = r"C:\Program Files (x86)\WinFsp\bin";

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn child_path() -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    format!("{WINFSP_BIN};{path}")
}

fn runner() -> &'static str {
    env!("CARGO_BIN_EXE_winfsp_run")
}

fn fresh(sub: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("layerfs_winfsp_e2e_{sub}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn run(dir: &std::path::Path, args: &[&str]) -> Ran {
    let output = Command::new(runner())
        .arg(dir)
        .args(args)
        .env("PATH", child_path())
        .output()
        .expect("spawn winfsp_run");
    Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// One `cmd /c <command>` round at the mounted volume.
fn shell(dir: &std::path::Path, command: &str) -> Ran {
    run(dir, &["--", "cmd", "/c", command])
}

/// The volume's own listing lines: everything the runner itself prints is
/// dropped, so an entry named `d` cannot hide inside the word "mounted".
fn entries(ran: &Ran) -> Vec<String> {
    ran.stdout
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.starts_with("initialized")
                && !line.starts_with("mounted")
                && !line.starts_with("holding")
                && *line != "committed"
                && *line != "unmounted"
        })
        .map(str::to_owned)
        .collect()
}

#[test]
#[ignore = "needs WinFsp driver and built winfsp_run bin"]
fn cmd_semantics_matrix() {
    let dir = fresh("matrix");

    // 1. create + write
    let ran = shell(&dir, "echo alpha > a.txt");
    assert_eq!(
        ran.code,
        Some(0),
        "write a.txt: {}{}",
        ran.stdout,
        ran.stderr
    );
    // 2. read back
    let ran = shell(&dir, "type a.txt");
    assert_eq!(ran.code, Some(0));
    assert!(ran.stdout.contains("alpha"), "a.txt: {}", ran.stdout);
    // 3. append
    let ran = shell(&dir, "echo beta >> a.txt");
    assert_eq!(ran.code, Some(0));
    // 4. both lines persist across sessions
    let ran = shell(&dir, "type a.txt");
    assert!(ran.stdout.contains("alpha") && ran.stdout.contains("beta"));
    // 5. mkdir
    let ran = shell(&dir, "mkdir d");
    assert_eq!(ran.code, Some(0));
    let ran = shell(&dir, "dir /b");
    let listed = entries(&ran);
    assert!(
        listed.iter().any(|name| name == "d"),
        "dir after mkdir: {listed:?}"
    );
    // 6. write below the new directory
    let ran = shell(&dir, "echo x > d\\b.txt");
    assert_eq!(ran.code, Some(0), "write d\\b.txt: {}", ran.stderr);
    let ran = shell(&dir, "type d\\b.txt");
    assert_eq!(ran.code, Some(0));
    assert!(ran.stdout.contains('x'));
    // 7. rename
    let ran = shell(&dir, "ren a.txt c.txt");
    assert_eq!(ran.code, Some(0), "ren: {}", ran.stderr);
    let ran = shell(&dir, "type c.txt");
    assert_eq!(ran.code, Some(0));
    assert!(ran.stdout.contains("beta"));
    let ran = shell(&dir, "type a.txt");
    assert_ne!(ran.code, Some(0), "a.txt must be gone after ren");
    // 8. delete
    let ran = shell(&dir, "del c.txt");
    assert_eq!(ran.code, Some(0));
    let ran = shell(&dir, "type c.txt");
    assert_ne!(ran.code, Some(0), "c.txt must be gone after del");
    // 9. rmdir of a non-empty directory fails; emptying it lets it through
    let ran = shell(&dir, "rmdir d");
    assert_ne!(
        ran.code,
        Some(0),
        "rmdir of non-empty d must fail: {}{}",
        ran.stdout,
        ran.stderr
    );
    let ran = shell(&dir, "del d\\b.txt");
    assert_eq!(ran.code, Some(0));
    let ran = shell(&dir, "rmdir d");
    assert_eq!(ran.code, Some(0), "rmdir d: {}", ran.stderr);
    let ran = shell(&dir, "dir /b");
    let listed = entries(&ran);
    assert!(
        !listed.iter().any(|name| name == "d"),
        "d must be gone: {listed:?}"
    );
    // 10. a no-newline write followed by an append keeps the bytes exact.
    // PowerShell runs directly: through `cmd /c` the nested quotes would be
    // re-parsed into a different command.
    let ran = run(
        &dir,
        &[
            "--",
            "powershell",
            "-NoProfile",
            "-Command",
            "Set-Content -NoNewline -Path z.txt -Value ab",
        ],
    );
    assert_eq!(ran.code, Some(0), "Set-Content: {}", ran.stderr);
    let ran = shell(&dir, "echo c >> z.txt");
    assert_eq!(ran.code, Some(0));
    let ran = shell(&dir, "type z.txt");
    assert!(ran.stdout.contains("abc"), "z.txt: {:?}", ran.stdout);
    // 11. names Windows itself could never hold are refused (A1)
    let ran = shell(&dir, "echo x > bad:name.txt");
    assert_ne!(ran.code, Some(0), "colon name must be refused");
    let ran = shell(&dir, "echo x > CON.txt");
    assert_ne!(ran.code, Some(0), "reserved device name must be refused");
    // 12. exactly the surviving file remains, across all these sessions
    let ran = shell(&dir, "dir /b");
    assert_eq!(entries(&ran), vec!["z.txt".to_owned()], "final listing");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs WinFsp driver and built winfsp_run bin"]
fn killed_runner_leaves_reusable_store() {
    let dir = fresh("kill");
    let ran = shell(&dir, "echo seed > z.txt");
    assert_eq!(
        ran.code,
        Some(0),
        "seed write: {}{}",
        ran.stdout,
        ran.stderr
    );

    // Hold the mount with no command, until "mounted" is observed.
    let mut child = Command::new(runner())
        .arg(&dir)
        .args(["--idle", "60"])
        .env("PATH", child_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn idle runner");
    let mut saw_mounted = false;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if line.contains("mounted") {
                saw_mounted = true;
                break;
            }
        }
    }
    if !saw_mounted {
        let finished = child.wait_with_output().expect("collect idle runner");
        panic!(
            "mount line not observed; exit {:?}, stderr: {}",
            finished.status.code(),
            String::from_utf8_lossy(&finished.stderr)
        );
    }

    let killed = Command::new("taskkill")
        .args(["/F", "/PID", &child.id().to_string()])
        .output()
        .expect("taskkill");
    assert!(killed.status.success(), "taskkill failed");
    let _ = child.wait();
    std::thread::sleep(Duration::from_secs(2));

    // The Store survives the kill: a later round reads the committed content
    // and the stale mount directory is cleaned by the new attach.
    let ran = shell(&dir, "type z.txt");
    assert_eq!(
        ran.code,
        Some(0),
        "after kill: {}{}",
        ran.stdout,
        ran.stderr
    );
    assert!(ran.stdout.contains("seed"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "needs WinFsp driver and built winfsp_run bin"]
fn second_runner_on_same_store_fails() {
    let dir = fresh("mutex");

    let mut first: Child = Command::new(runner())
        .arg(&dir)
        .args(["--idle", "4"])
        .env("PATH", child_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn first runner");
    std::thread::sleep(Duration::from_millis(1500));

    let second = shell(&dir, "echo two > t.txt");
    assert_ne!(
        second.code,
        Some(0),
        "one writer only: second runner must fail: {}{}",
        second.stdout,
        second.stderr
    );
    let exit = first.wait().expect("wait first runner");
    assert!(exit.success(), "first runner finishes cleanly: {exit}");

    let _ = std::fs::remove_dir_all(&dir);
}
