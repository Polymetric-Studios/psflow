//! CLI integration tests — verify the psflow binary works end-to-end.

use std::process::Command;

fn psflow() -> Command {
    Command::new(env!("CARGO_BIN_EXE_psflow"))
}

fn write_temp_mmd(content: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::with_suffix(".mmd").unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f
}

#[test]
fn help_flag_exits_success() {
    let output = psflow().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("psflow"));
    assert!(stdout.contains("--validate"));
}

#[test]
fn version_flag_exits_success() {
    let output = psflow().arg("--version").output().unwrap();
    assert!(output.status.success());
}

#[test]
fn missing_file_exits_failure() {
    let output = psflow().arg("nonexistent.mmd").output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot read"));
}

#[test]
fn validate_valid_graph() {
    let f = write_temp_mmd(
        "\
graph TD
    A[Start] --> B[End]

    %% @A handler: passthrough
    %% @B handler: passthrough
",
    );

    let output = psflow()
        .arg("--validate")
        .arg(f.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("validation passed"));
}

#[test]
fn execute_simple_graph() {
    let f = write_temp_mmd(
        "\
graph TD
    A[Start] --> B[End]

    %% @A handler: passthrough
    %% @B handler: passthrough
",
    );

    let output = psflow().arg(f.path()).output().unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("completed in"));
}

#[test]
fn json_output_is_valid_json() {
    let f = write_temp_mmd(
        "\
graph TD
    A[Start] --> B[End]

    %% @A handler: passthrough
    %% @B handler: passthrough
",
    );

    let output = psflow().arg("--json").arg(f.path()).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.get("nodes").is_some());
}

#[test]
fn empty_file_executes_as_empty_graph() {
    let f = write_temp_mmd("");

    let output = psflow().arg(f.path()).output().unwrap();
    // Empty graph is valid — executes with 0 nodes
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("0 nodes"));
}

#[test]
fn verbose_flag_produces_debug_output() {
    let f = write_temp_mmd(
        "\
graph TD
    A[Start] --> B[End]

    %% @A handler: passthrough
    %% @B handler: passthrough
",
    );

    let output = psflow().arg("-vv").arg(f.path()).output().unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Debug-level output should contain wave processing info
    assert!(stderr.contains("wave") || stderr.contains("DEBUG"));
}
