use tempfile::tempdir;
use wokcore_platform::process_matches_executable;

#[test]
fn process_executable_identity_requires_the_exact_current_image() {
    let current = std::env::current_exe().unwrap();
    assert!(process_matches_executable(std::process::id(), &current));

    let directory = tempdir().unwrap();
    let wrong = directory.path().join(if cfg!(windows) {
        "wokcore.exe"
    } else {
        "wokcore"
    });
    std::fs::write(&wrong, b"not the current process").unwrap();

    assert!(!process_matches_executable(std::process::id(), &wrong));
    assert!(!process_matches_executable(0, &current));
}
