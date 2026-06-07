use std::process::{Command, Stdio};

fn repo_script() -> String {
    let repo_root = env!("CARGO_MANIFEST_DIR");
    std::fs::read_to_string(format!("{repo_root}/scripts/install.sh")).expect("read install.sh")
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fake_package_root(name: &str, shell_name: &str) -> std::path::PathBuf {
    let root = temp_dir(name);
    let cdh = root.join("cdh");
    std::fs::write(&cdh, "#!/usr/bin/env sh\nexit 0\n").expect("write fake cdh");

    let installer_dir = root.join(format!("scripts/installers/{shell_name}"));
    std::fs::create_dir_all(&installer_dir).expect("create fake installer dir");
    std::fs::write(
        installer_dir.join("install.sh"),
        "#!/usr/bin/env bash\nset -Eeuo pipefail\necho fake child install\n",
    )
    .expect("write fake child installer");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cdh, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake cdh");
    }

    root
}

fn write_repo_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script_path = dir.join("install.sh");
    std::fs::write(&script_path, repo_script()).expect("write repo script copy");
    script_path
}

#[test]
fn uninstall_accepts_stdin_piped_installer_with_unset_bash_source() {
    let script = repo_script();
    let temp_home = temp_dir("cdh-install-test");

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

#[test]
fn install_without_tty_accepts_explicit_shell_before_downloading() {
    let script = repo_script();
    let temp_home = temp_dir("cdh-install-explicit-shell-test");
    let package_root = fake_package_root("cdh-install-explicit-package-test", "bash");

    let mut child = Command::new("bash")
        .args(["--noprofile", "--norc", "-s", "--", "--shell", "bash"])
        .env("HOME", &temp_home)
        .env("SHELL", "/bin/unsupported")
        .env("CDH_PACKAGE_ROOT", &package_root)
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
    let _ = std::fs::remove_dir_all(&package_root);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("[cdh] 使用指定 shell：bash"),
        "installer should resolve explicit shell before network work\noutput:\n{combined}"
    );
    assert!(
        combined.contains("fake child install"),
        "installer should use fake package child installer\noutput:\n{combined}"
    );
}

#[test]
fn install_without_tty_fails_before_download_when_shell_is_unknown() {
    let script = repo_script();
    let temp_home = temp_dir("cdh-install-unknown-shell-test");

    let mut child = Command::new("bash")
        .args(["--noprofile", "--norc", "-s"])
        .env("HOME", &temp_home)
        .env("SHELL", "/bin/unsupported")
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
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "installer should fail without a target shell"
    );
    assert!(
        combined.contains("无法自动识别支持的 shell"),
        "installer should explain unsupported SHELL\noutput:\n{combined}"
    );
    assert!(
        !combined.contains("获取二进制"),
        "installer should fail before downloading binary\noutput:\n{combined}"
    );
}

#[test]
fn docs_install_wrapper_forwards_arguments_to_main_installer() {
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let temp_home = temp_dir("cdh-docs-wrapper-home-test");
    let script_dir = temp_dir("cdh-docs-wrapper-script-test");
    let script_path = write_repo_script(&script_dir);
    let package_root = fake_package_root("cdh-docs-wrapper-package-test", "bash");

    let output = Command::new("bash")
        .arg(format!("{repo_root}/docs/install.sh"))
        .args(["--shell", "bash"])
        .env("HOME", &temp_home)
        .env("SHELL", "/bin/unsupported")
        .env(
            "CDH_INSTALL_URL",
            format!("file://{}", script_path.display()),
        )
        .env("CDH_PACKAGE_ROOT", &package_root)
        .output()
        .expect("run docs wrapper");

    let _ = std::fs::remove_dir_all(&temp_home);
    let _ = std::fs::remove_dir_all(&script_dir);
    let _ = std::fs::remove_dir_all(&package_root);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        output.status.success(),
        "docs wrapper should succeed\noutput:\n{combined}"
    );
    assert!(
        combined.contains("[cdh] 使用指定 shell：bash"),
        "docs wrapper should forward --shell argument\noutput:\n{combined}"
    );
    assert!(
        combined.contains("fake child install"),
        "docs wrapper should execute the main installer\noutput:\n{combined}"
    );
}
