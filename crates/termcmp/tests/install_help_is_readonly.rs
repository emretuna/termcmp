//! Regression tests pinning read-only invocations on write-capable commands.
//!
//! `install --help` and `install --dry-run` must never mutate the caller's
//! HOME. These tests run with an isolated HOME so a regression cannot touch
//! the caller's real shell files.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

fn ghost_bin() -> PathBuf {
    env!("CARGO_BIN_EXE_termcmp").into()
}

fn command_with_isolated_home(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(ghost_bin());
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
    cmd
}

#[test]
fn install_help_does_not_write_zshrc() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();

    let output = command_with_isolated_home(home)
        .arg("install")
        .arg("--help")
        .output()
        .expect("spawn termcmp");

    assert!(
        output.status.success(),
        "install --help should exit 0; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("install") && stdout.to_lowercase().contains("help"),
        "expected install help text; got:\n{stdout}",
    );

    assert!(
        !home.join(".zshrc").exists(),
        "install --help must NOT create ~/.zshrc",
    );
    assert!(
        !home.join(".config/termcmp").exists(),
        "install --help must NOT create ~/.config/termcmp/",
    );
    assert!(
        !home.join(".backup.termcmp").exists(),
        "install --help must NOT create ~/.backup.termcmp",
    );
}

#[test]
fn uninstall_help_does_not_write_zshrc() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();

    let output = command_with_isolated_home(home)
        .arg("uninstall")
        .arg("--help")
        .output()
        .expect("spawn termcmp");

    assert!(
        output.status.success(),
        "uninstall --help should exit 0; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !home.join(".zshrc").exists(),
        "uninstall --help must NOT create ~/.zshrc",
    );
    assert!(
        !home.join(".config/termcmp").exists(),
        "uninstall --help must NOT create ~/.config/termcmp/",
    );
    assert!(
        !home.join(".backup.termcmp").exists(),
        "uninstall --help must NOT create ~/.backup.termcmp",
    );
}

#[test]
fn install_dry_run_through_clap_does_not_write() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();

    // Seed a .zshrc so the install target exists (dry-run still must not write)
    let zshrc = home.join(".zshrc");
    std::fs::write(&zshrc, "export EXISTING=1\n").unwrap();

    let output = command_with_isolated_home(home)
        .arg("install")
        .arg("--dry-run")
        .output()
        .expect("spawn termcmp");

    assert!(
        output.status.success(),
        "install --dry-run should exit 0; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Dry run:"),
        "expected dry-run banner in stdout; got:\n{stdout}",
    );

    assert_eq!(
        std::fs::read_to_string(&zshrc).unwrap(),
        "export EXISTING=1\n",
        "install --dry-run must NOT modify ~/.zshrc",
    );
    assert!(
        !home.join(".config/termcmp/shell").exists(),
        "install --dry-run must NOT create ~/.config/termcmp/shell",
    );
    assert!(
        !home.join(".config/termcmp/specs").exists(),
        "install --dry-run must NOT create ~/.config/termcmp/specs",
    );
    assert!(
        !home.join(".config/termcmp/config.toml").exists(),
        "install --dry-run must NOT create ~/.config/termcmp/config.toml",
    );
}

#[test]
fn install_halts_when_no_shell_rc_files() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path();

    let output = command_with_isolated_home(home)
        .arg("install")
        .output()
        .expect("spawn termcmp");

    assert!(
        !output.status.success(),
        "install must fail when no rc files exist; got {:?}",
        output.status,
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        combined.contains(".zshrc"),
        "error must name .zshrc:\n{combined}"
    );
    assert!(
        combined.contains(".bashrc"),
        "error must name .bashrc:\n{combined}"
    );
    assert!(
        combined.contains("config.fish"),
        "error must name config.fish:\n{combined}"
    );

    // Nothing was written before the halt
    assert!(
        !home.join(".config/termcmp").exists(),
        "halt must happen before any config writes"
    );
    assert!(!home.join(".zshrc").exists());
    assert!(!home.join(".bashrc").exists());
}
