//! Runs the startup repairs on a COPY of a data folder: `rehearse_repair <DATA_ROOT>`.
#![allow(clippy::expect_used, clippy::print_stdout)]

use std::path::PathBuf;

fn main() {
    let data = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: rehearse_repair <DATA_ROOT>"),
    );
    println!(
        "sign-in identity notes written: {}",
        connecto_session::launch::repair_secure_storage(&data)
    );
    println!(
        "conversation files brought into empty profiles: {}",
        connecto_session::launch::repair_empty_generations(&data)
    );
}
