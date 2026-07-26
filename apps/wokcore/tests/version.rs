use std::process::Command;

#[test]
fn version_flag_reports_package_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_wokcore"))
        .arg("--version")
        .output()
        .expect("wokcore binary should start");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        concat!("wokcore ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert!(output.stderr.is_empty());
}
