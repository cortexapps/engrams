//! Test-only supervisor for the Linux MCP entrypoint gate. Cargo examples are
//! not production binaries and are not selected by default nextest discovery.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux MCP supervisor requires Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::sys::prctl::set_child_subreaper;
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    const CLEANUP: Duration = Duration::from_secs(10);
    const TICK: Duration = Duration::from_millis(10);

    fn private_dir(path: &Path) -> Result<()> {
        fs::create_dir(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn driver(root: &Path) -> Result<Command> {
        let binary = PathBuf::from(std::env::var("ENGRAM_TEST_DRIVER_BIN")?);
        if !binary.is_absolute() || !binary.is_file() {
            return Err("current test driver is required".into());
        }
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("ENGRAM_TEST_STATE_ROOT", root)
            .args(["--ignored", "--test-threads=1"])
            .stdin(Stdio::null());
        for name in [
            "ENGRAM_TEST_CLAUDE_BIN",
            "ENGRAM_TEST_CODEX_BIN",
            "ENGRAM_FIXTURE_BIN",
        ] {
            command.env(
                name,
                std::env::var_os(name).ok_or("required artifact missing")?,
            );
        }
        Ok(command)
    }

    fn wait_until(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            sleep(TICK);
        }
    }

    fn kill_driver(child: &mut Child) -> Result<ExitStatus> {
        child.kill()?;
        wait_until(child, Instant::now() + CLEANUP)?.ok_or_else(|| "driver did not exit".into())
    }

    /// This program is single-threaded and creates only the owned test driver.
    /// After its death, Linux reparents orphan descendants to this subreaper.
    /// Kill only our kernel-reported direct children; no PID file or name scan
    /// supplies kill targets. Unreaped children reserve their PID until waitpid.
    fn drain_children() -> Result<()> {
        let deadline = Instant::now() + CLEANUP;
        let children = format!("/proc/self/task/{}/children", std::process::id());
        loop {
            for value in fs::read_to_string(&children)?.split_whitespace() {
                let pid = Pid::from_raw(value.parse()?);
                match kill(pid, Signal::SIGKILL) {
                    Ok(()) | Err(Errno::ESRCH) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            loop {
                match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                    Err(Errno::ECHILD) => return Ok(()),
                    Ok(WaitStatus::StillAlive) => break,
                    Ok(_) | Err(Errno::EINTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            // /proc children can omit entries during concurrent exits. Only
            // ECHILD, never an empty snapshot, confirms complete cleanup.
            if Instant::now() >= deadline {
                return Err("owned descendants did not exit".into());
            }
            sleep(TICK);
        }
    }

    fn stat(pid: u32) -> Result<Vec<String>> {
        let value = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        Ok(value
            .rsplit_once(") ")
            .ok_or("invalid process stat")?
            .1
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    }

    fn forced_deadline(root: &Path) -> Result<()> {
        private_dir(root)?;
        let mut child = driver(root)?
            .args(["supervisor_timeout_fixture", "--exact", "--nocapture"])
            .spawn()?;
        let ready = root.join("cleanup-ready");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() {
            if child.try_wait()?.is_some() {
                return Err("cleanup fixture exited before readiness".into());
            }
            if Instant::now() >= deadline {
                return Err("cleanup fixture readiness deadline".into());
            }
            sleep(TICK);
        }
        let pairs: Vec<[u32; 2]> = serde_json::from_slice(&fs::read(&ready)?)?;
        if pairs.len() != 2 {
            return Err("both held wrappers are required".into());
        }
        let mut observed = Vec::new();
        for [wrapper, fixture] in pairs {
            let wrapper_stat = stat(wrapper)?;
            let fixture_stat = stat(fixture)?;
            if wrapper_stat[1].parse::<u32>()? != child.id()
                || wrapper_stat[2].parse::<u32>()? != wrapper
                || fixture_stat[1].parse::<u32>()? != wrapper
                || fixture_stat[2].parse::<u32>()? != wrapper
            {
                return Err("cleanup fixture ownership or separate process group mismatch".into());
            }
            // starttime distinguishes the observed child from later PID reuse.
            observed.push((wrapper, wrapper_stat[19].clone()));
            observed.push((fixture, fixture_stat[19].clone()));
        }
        // Negative control: reproduce the old deadline that killed only the
        // test process. SIGKILL cannot run Rust Drop. Its separate groups live.
        if wait_until(&mut child, Instant::now() + Duration::from_millis(100))?.is_some() {
            return Err("cleanup fixture did not reach its forced deadline".into());
        }
        let status = kill_driver(&mut child)?;
        if status.signal() != Some(9) {
            return Err("test driver was not SIGKILLed".into());
        }
        for (pid, started) in &observed {
            let current = stat(*pid)?;
            if &current[19] != started || current[0] == "Z" {
                return Err("negative control did not leave the owned descendant alive".into());
            }
        }
        if !ready.exists() {
            return Err("state removed before descendant cleanup".into());
        }
        println!(
            "forced deadline control: killed test; four owned descendants alive; state retained"
        );
        drain_children()?;
        for (pid, started) in observed {
            if let Ok(current) = stat(pid) {
                if current[19] == started {
                    return Err("observed descendant remains after cleanup".into());
                }
            }
        }
        if !ready.exists() {
            return Err("state removed before cleanup confirmation".into());
        }
        fs::remove_dir_all(root)?;
        println!("forced deadline cleanup: passed; all descendants reaped before state removal");
        Ok(())
    }

    fn run(root: &Path) -> Result<()> {
        forced_deadline(&root.join("probe"))?;
        let matrix = root.join("matrix");
        private_dir(&matrix)?;
        let mut child = driver(&matrix)?
            .args(["--skip", "supervisor_timeout_fixture"])
            .spawn()?;
        let status = match wait_until(&mut child, Instant::now() + Duration::from_secs(100))? {
            Some(status) => status,
            None => {
                kill_driver(&mut child)?;
                return Err("Linux wrapper matrix deadline".into());
            }
        };
        if !status.success() {
            return Err("Linux wrapper matrix failed".into());
        }
        Ok(())
    }

    pub fn main() -> Result<()> {
        if std::env::args().skip(1).collect::<Vec<_>>() != ["--check-cleanup-and-run"] {
            return Err("explicit cleanup check and matrix mode required".into());
        }
        // Establish ownership BEFORE the test driver can spawn any descendants.
        set_child_subreaper(true)?;
        let base = PathBuf::from(std::env::var("ENGRAM_TEST_STATE_ROOT")?);
        if !base.is_absolute() || base.canonicalize()? != base {
            return Err("owned canonical state root required".into());
        }
        let root = base.join("runtime");
        private_dir(&root)?;
        let result = run(&root);
        // Also runs on fixture/assertion failure. If cleanup cannot be proved,
        // leave state and no marker; the shell's hard deadline cannot erase it.
        drain_children()?;
        fs::remove_dir_all(&root)?;
        fs::write(base.join("cleanup-complete"), b"complete\n")?;
        result
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux::main() {
        eprintln!("Linux wrapper supervisor failed: {error}");
        std::process::exit(1);
    }
}
