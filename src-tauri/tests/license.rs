//! `pullkit --license` run as a user would run it.

use std::process::Command;

#[test]
fn license_flag_prints_the_licenses_and_exits_zero() {
    // Arrange
    // An empty home, so that a regression which reached the configuration
    // would create a file there rather than in the real one.
    let home = std::env::temp_dir().join(format!("pullkit-license-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    // Act
    let output = Command::new(env!("CARGO_BIN_EXE_pullkit"))
        .arg("--license")
        .env("HOME", &home)
        .output()
        .unwrap();

    // Assert
    let home_entries = std::fs::read_dir(&home).unwrap().count();
    std::fs::remove_dir_all(&home).unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("MIT License"), "{stdout}");
    assert!(stdout.contains("THIRD-PARTY NOTICES"));
    assert!(stdout.contains("  tauri "));
    assert_eq!(home_entries, 0, "--license touched the configuration");
}
