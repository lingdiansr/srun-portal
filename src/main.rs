//! `portal`-equivalent entry point: `srun-portal [portal-url]` (SPEC §3.1).

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(srun_portal::cli::run(argv));
}
