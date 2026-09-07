#[test]
fn original_directory_publishing_and_previous_archive_recovery_contract() {
    let result = std::process::Command::new("python")
        .arg("tests/inplace_contract.py")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Python 3 is required for the shipped remote executor contract");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
