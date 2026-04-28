//! `engram-uffd-handler` — page-fault handler binary.
//!
//! Linux-only at runtime; on other platforms the binary still
//! compiles (it's part of a workspace `cargo check` that may run
//! on macOS) but exits 2 with a clear message.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("engram-uffd-handler: userfaultfd is a Linux kernel feature; this binary is a no-op on other platforms");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::path::PathBuf;
    use std::process::ExitCode;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    match engram_uffd_handler::run_listener(args.listen, args.memory_bin) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engram-uffd-handler: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(target_os = "linux")]
struct Args {
    listen: std::path::PathBuf,
    memory_bin: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
fn parse_args() -> Result<Args, String> {
    use std::path::PathBuf;
    let mut listen: Option<PathBuf> = None;
    let mut memory_bin: Option<PathBuf> = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--listen" => {
                let v = argv
                    .next()
                    .ok_or_else(|| "--listen requires a value".to_string())?;
                listen = Some(PathBuf::from(v));
            }
            "--memory-bin" => {
                let v = argv
                    .next()
                    .ok_or_else(|| "--memory-bin requires a value".to_string())?;
                memory_bin = Some(PathBuf::from(v));
            }
            "-h" | "--help" => {
                eprintln!(
                    "engram-uffd-handler --listen <sock> --memory-bin <path>\n\n\
                     UFFD page-fault handler. Firecracker connects to <sock>,\n\
                     hands over the guest's UFFD, and the handler serves\n\
                     faults from <memory.bin> on demand."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let listen = listen.ok_or_else(|| "--listen <sock> is required".to_string())?;
    let memory_bin = memory_bin.ok_or_else(|| "--memory-bin <path> is required".to_string())?;
    Ok(Args { listen, memory_bin })
}
