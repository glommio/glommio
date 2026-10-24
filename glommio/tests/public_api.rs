#[cfg(feature = "version-compatibility")]
mod public_api_snapshot {
    const NIGHTLY: &str = "nightly-2026-06-11";
    const SNAPSHOT_PATH: &str = "./tests/snapshots/public-api.txt";

    /// Verifies that the public API matches the committed snapshot.
    ///
    /// Run with `UPDATE_SNAPSHOTS=yes` to refresh the snapshot after intentional
    /// API changes.
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
        public_api.assert_eq_or_update(SNAPSHOT_PATH);
    }
}
