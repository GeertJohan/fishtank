use anyhow::Result;
use vergen_gix::{BuildBuilder, CargoBuilder, Emitter, GixBuilder};

fn main() -> Result<()> {
    let build = BuildBuilder::all_build()?;
    let cargo = CargoBuilder::all_cargo()?;

    let mut emitter = Emitter::default();
    emitter.add_instructions(&build)?.add_instructions(&cargo)?;

    // Only pull git info when the repo actually has a commit. On a freshly
    // init'd repo (no commits) vergen would otherwise emit a pile of
    // "VERGEN_GIT_* set to default" fallback warnings on every build.
    if git_has_commits() {
        emitter.add_instructions(&GixBuilder::all_git()?)?;
    } else {
        // Keep VERGEN_GIT_DESCRIBE defined so cli.rs still compiles, and rebuild
        // once the first commit lands so the real value gets picked up.
        println!("cargo:rustc-env=VERGEN_GIT_DESCRIBE=unknown");
        println!("cargo:rerun-if-changed=.git/HEAD");
    }

    emitter.emit()
}

fn git_has_commits() -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
