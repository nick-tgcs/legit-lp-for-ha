//! Thin binary wrapper; all logic lives in the library (see lib.rs).

fn main() -> anyhow::Result<()> {
    println!(
        "legit-lp-scheduler {} — solve loop arrives in milestone 10",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}
