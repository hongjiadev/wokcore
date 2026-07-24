#[test]
fn workspace_exposes_wokcore_build_identity() {
    let info = wokcore_core::build::BuildInfo::current();
    assert_eq!(info.product, "WokCore");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
}
