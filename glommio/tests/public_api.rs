const NIGHTLY: &str = "nightly";
const SNAPSHOT_PATH: &str = "./tests/snapshots/public-api.txt";

#[test]
fn public_api() {
    rustup_toolchain::install(NIGHTLY).unwrap();
    let rustdoc_json = rustdoc_json::Builder::default()
        .toolchain(NIGHTLY)
        .build()
        .unwrap();
    let public_api = public_api::Builder::from_rustdoc_json(rustdoc_json)
        .build()
        .unwrap();
    // Run with env var `UPDATE_SNAPSHOTS=yes` to update the snapshot.
    public_api.assert_eq_or_update(SNAPSHOT_PATH);
}
