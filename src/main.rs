use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = ezthumb::cli::run_cli(&args);
    ExitCode::from(code.clamp(0, 255) as u8)
}
