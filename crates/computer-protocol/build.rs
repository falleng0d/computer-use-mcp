use std::env;
use std::path::Path;

const VERSION_ENV: &str = "COMPUTER_USE_MCP_VERSION";

fn main() {
    println!("cargo:rerun-if-env-changed={VERSION_ENV}");
    let given = env::var(VERSION_ENV).is_ok_and(|version| !version.is_empty());
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for build scripts");
    // Cargo writes this file only into packaged crates, so its presence means a crates.io build.
    if !given
        && Path::new(&manifest_dir)
            .join(".cargo_vcs_info.json")
            .exists()
    {
        let version =
            env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION for build scripts");
        println!("cargo:rustc-env={VERSION_ENV}={version}");
    }
}
