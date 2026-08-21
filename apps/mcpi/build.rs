//! Stamps the build date into the binary.
//!
//! The licence check compares a customer's update window against *when this
//! build was released*, never against the clock — so a build someone paid for
//! keeps working forever. That comparison needs the date to be fixed at compile
//! time; reading it at runtime would silently turn "one year of updates" into
//! "one year of use".
//!
//! This makes builds non-reproducible by design. That is the trade.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=MCPI_BUILD_UNIX={seconds}");
    // Without this the stamp would be cached from the first build forever.
    println!("cargo:rerun-if-changed=build.rs");
}
