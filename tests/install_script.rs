use std::process::{Command, Stdio};

#[test]
fn uninstall_accepts_stdin_piped_installer_with_unset_bash_source() {
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let script = std::fs::read_to_string(format!("{repo_root}/scripts/install.sh"))
        .expect("read install.sh");
    let temp_home = std::env::temp_dir().join(format!(
        "cdh-install-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&temp_home);
    std::fs::create_dir_all(&temp_home).expect("create temp home");

    let mut child = Command::new("bash")
        .args(["--noprofile", "--norc", "-s", "--", "--action", "uninstall"])
        .env("HOME", &temp_home)
        .env_remove("CDH_PACKAGE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("open stdin");
        stdin.write_all(script.as_bytes()).expect("pipe installer");
    }

    let output = child.wait_with_output().expect("wait for installer");
    let _ = std::fs::remove_dir_all(&temp_home);

    assert!(
        output.status.success(),
        "installer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("BASH_SOURCE"),
        "stderr should not mention BASH_SOURCE"
    );
}
