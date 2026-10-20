//! engrams — the product CLI. Talks ONLY to the orchestrator (Connect RPC on
//! /rpc + the SSE/auth HTTP routes); the coordinator is an internal service
//! the CLI never dials. Auth is `engrams auth login` (device flow → stored
//! user-owned API key) or ENGRAMS_API_KEY (CI). See the crate README.
//!
//! Output contract (carried over from the Bun CLI so script pipelines port
//! unchanged): default = human tables; --json = pretty JSON on stdout and
//! nothing else; errors on stderr as `engrams: …` + exit 1.

mod commands;
mod output;
mod status;

use clap::{Args, Parser, Subcommand};

use engram_app_client::config::{resolve_api_key, resolve_host};
use engram_app_client::{Auth, Clients, Transport};
use output::{fail, CliError, CliResult};

/// The release version. `scripts/release-version.sh` rewrites this line.
const VERSION: &str = "0.11.0";

/// The version the binary reports. A nightly build (nightly.yml) sets
/// `ENGRAMS_BUILD_VERSION` to its prerelease version, for example
/// `0.12.0-rc.3`; every other build reports the release version.
const BUILD_VERSION: &str = match option_env!("ENGRAMS_BUILD_VERSION") {
    Some(v) => v,
    None => VERSION,
};

#[derive(Parser, Debug)]
#[command(
    name = "engrams",
    about = "engrams product CLI — sessions, tasks, images, fleet",
    disable_version_flag = true
)]
struct Cli {
    /// orchestrator base URL (env: ENGRAMS_URL)
    #[arg(long, global = true, value_name = "url")]
    url: Option<String>,
    /// print raw JSON instead of the human-readable view
    #[arg(long, global = true)]
    json: bool,
    /// print the version
    #[arg(long)]
    version: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// log in / out of an engrams host
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// tasks — profile-based agent runs
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// operations on sessions
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// operations on enabled images
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Docker registry credentials (envelope-encrypted server-side)
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    /// operations on fleet hosts
    #[command(alias = "hosts")]
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// session profiles (read-only)
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// global service-account API keys (admin; your login key is `auth`)
    Apikey {
        #[command(subcommand)]
        cmd: ApikeyCmd,
    },
    /// explicit triggers for implicitly-driven primitives
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// attach a terminal UI to a session
    Tui {
        /// session to attach to; omit for the session list
        session_id: Option<String>,
        /// keep the terminal's own scrollback: no alternate screen
        #[arg(long)]
        no_alt_screen: bool,
        /// do not capture the mouse (wheel scrolling off; native selection on)
        #[arg(long)]
        no_mouse: bool,
        /// screen-reader mode: native scrollback, word labels, no mouse
        #[arg(long)]
        ax: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// authenticate via your browser (device flow) and store an API key
    Login,
    /// revoke the stored key for this host and forget it
    Logout,
    /// show who you are logged in as
    Status,
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    /// start a task from a profile (prints the primary session id)
    Create(commands::task::CreateOpts),
    /// list your tasks (admins additionally see unattributed sessions)
    List,
    /// print one task
    Get { id: String },
    /// delete a task AND tear down its session(s)
    Delete { id: String },
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// admin escape hatch: boot a session from a raw image URI (no profile)
    Create(commands::session::CreateOpts),
    /// list sessions
    List,
    /// print one session
    Get { id: String },
    /// run a shell command in the session's sandbox (mirrors its exit code)
    Exec {
        id: String,
        cmd: String,
        /// wall-clock timeout
        #[arg(long, value_name = "n")]
        timeout_secs: Option<u64>,
    },
    /// mark the session completed and tear down its sandbox
    Delete { id: String },
    /// an interactive shell in the session's sandbox (exit it to return)
    Shell { id: String },
    /// tail the persistent event log (SSE; Ctrl-C to stop)
    Logs {
        id: String,
        /// replay strictly-after idx N, then tail
        #[arg(long, value_name = "idx")]
        since: Option<i64>,
    },
    /// show the conversation timeline (newest rows by default)
    Log {
        id: String,
        /// cap on rows returned
        #[arg(long, value_name = "n")]
        limit: Option<i64>,
        /// oldest rows instead of the tail
        #[arg(long)]
        from_start: bool,
    },
    /// resume an Idle session via its hot snapshot
    Resume { id: String },
    /// push a prompt to a running session (auto-resumes Idle)
    Prompt { id: String, text: String },
}

/// The image config fields the CLI exposes. Everything else (the warm hook)
/// is set in the dashboard.
#[derive(Args, Debug, Clone, Default)]
pub struct ConfigFlags {
    /// display name shown in pickers (required on first enable)
    #[arg(long, value_name = "name")]
    name: Option<String>,
    /// what the image is for
    #[arg(long, value_name = "text")]
    description: Option<String>,
    /// working directory; defaults to the Dockerfile WORKDIR
    #[arg(long, value_name = "path")]
    workdir: Option<String>,
    /// guest vCPUs (required on first enable)
    #[arg(long, value_name = "n")]
    vcpus: Option<String>,
    /// guest memory in MiB
    #[arg(long, value_name = "n")]
    memory_mib: Option<String>,
    /// disk budget in GiB
    #[arg(long, value_name = "n")]
    disk_gib: Option<String>,
    /// ephemeral swap in MiB; 0 for none
    #[arg(long, value_name = "n")]
    swap_mib: Option<String>,
    /// non-secret env for every session (repeatable)
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    /// list the enabled images sessions may reference
    List,
    /// enable an image (captures its base snapshot; polls to ready). The warm hook is set in the dashboard under Operator → Images.
    Enable {
        /// full OCI URI: <host>[:port]/<repo>:<tag>
        #[arg(long, value_name = "uri")]
        uri: String,
        #[command(flatten)]
        config: ConfigFlags,
        /// print the job id and return without polling
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
    /// print an enabled image's stored config as JSON
    Config {
        /// full OCI URI of an enabled image
        #[arg(long, value_name = "uri")]
        uri: String,
    },
    /// change an enabled image's config; unset flags keep their stored values
    Update {
        /// full OCI URI of an already-enabled image
        #[arg(long, value_name = "uri")]
        uri: String,
        #[command(flatten)]
        config: ConfigFlags,
        /// consent to a recapture when the change needs one
        #[arg(long)]
        allow_recapture: bool,
        /// don't poll a recapture job to completion
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
    /// disable an image (registry artifact untouched)
    Disable {
        #[arg(long, value_name = "uri")]
        uri: String,
    },
    /// re-fetch a moved tag (and optionally force a recapture)
    Refresh {
        #[arg(long, value_name = "uri")]
        uri: String,
        /// force a new base-snapshot capture
        #[arg(long)]
        recapture: bool,
    },
    /// list recent enable/refresh jobs
    Jobs,
    /// print one enable/refresh job
    Job { id: String },
    /// poll a job until ready/failed
    PollJob { id: String },
    /// re-queue a failed enable/refresh job
    RetryJob {
        id: String,
        /// don't poll the retried job to completion
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RegistryCmd {
    /// add or update a registry credential
    Add(commands::image::RegistryAddOpts),
    /// list registry credentials (never returns passwords)
    List,
    /// remove the credential for a registry host
    Rm { host: String },
}

#[derive(Subcommand, Debug)]
enum HostCmd {
    /// list hosts
    List,
    /// print one host
    Get { id: String },
    /// flip a host to draining (new sessions avoid it)
    Drain { id: String },
    /// deregister a host row (refused while sessions are bound — drain / reap first)
    Delete { id: String },
    /// clear an admin cordon; host returns to ready
    Uncordon { id: String },
    /// evacuate one session off its host (scanner resumes it on a peer)
    Evacuate {
        #[arg(value_name = "session-id")]
        session_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileCmd {
    /// list launchable profiles
    List,
    /// print one profile
    Get { id: String },
}

#[derive(Subcommand, Debug)]
enum ApikeyCmd {
    /// mint a global key (plaintext printed once, alone, on stdout)
    Create {
        /// display name, e.g. "ci-bot"
        #[arg(long, value_name = "name")]
        name: String,
        /// admin | user
        #[arg(long, value_name = "role", default_value = "user")]
        role: String,
        /// ISO-8601 expiry; empty = never
        #[arg(long, value_name = "iso", default_value = "")]
        expires_at: String,
    },
    /// list all keys (masked preview only)
    List,
    /// revoke a key (idempotent)
    Revoke { id: String },
}

#[derive(Subcommand, Debug)]
enum AdminCmd {
    /// force an immediate chunked-disk flush of one session
    Flush {
        #[arg(value_name = "session-id")]
        session_id: String,
    },
    /// run the idle-eviction pipeline now (synchronous; session ends Idle)
    EvictIdle {
        #[arg(value_name = "session-id")]
        session_id: String,
    },
    /// blob GC sweeps (bundle/snapshot-blob/chunk); dry-run unless --apply
    Gc {
        /// mark candidates + delete promoted blobs (default: report only)
        #[arg(long)]
        apply: bool,
        /// override the candidate grace window in seconds (0 = delete same sweep)
        #[arg(long, value_name = "n")]
        grace_secs: Option<u64>,
    },
}

/// Global flags resolved at action time.
struct Ctx {
    host: String,
    json: bool,
}

impl Ctx {
    /// Clients for an authenticated command. Fails with the `auth login`
    /// pointer when no key is resolvable — every RPC verb calls through
    /// here, so the logged-out failure mode is one consistent message.
    fn clients(&self) -> CliResult<Clients> {
        let Some(key) = resolve_api_key(&self.host) else {
            return fail(format!(
                "not logged in to {} — run `engrams auth login` (or set ENGRAMS_API_KEY)",
                self.host
            ));
        };
        Ok(Clients::new(Transport::new(
            self.host.clone(),
            Auth::ApiKey(key),
        )))
    }
}

async fn run(cli: Cli) -> CliResult {
    use commands::*;
    let ctx = Ctx {
        host: resolve_host(cli.url.as_deref()),
        json: cli.json,
    };
    let json = ctx.json;
    let Some(command) = cli.command else {
        // commander prints help on stderr and exits 1 when no subcommand is
        // given.
        use clap::CommandFactory;
        eprintln!("{}", Cli::command().render_help());
        return Err(CliError::Exit(1));
    };
    match command {
        Commands::Auth { cmd } => match cmd {
            AuthCmd::Login => {
                let st = status::Status::new("Sign in");
                let r = auth::login(&ctx.host, &st).await;
                st.finish(&r, "Signed in", "Sign-in failed");
                r
            }
            AuthCmd::Logout => auth::logout(&ctx.host).await,
            AuthCmd::Status => auth::status(&ctx.host, json).await,
        },
        Commands::Task { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                TaskCmd::Create(o) => task::create(&c, o, json).await,
                TaskCmd::List => task::list(&c, json).await,
                TaskCmd::Get { id } => task::get(&c, &id, json).await,
                TaskCmd::Delete { id } => task::remove(&c, &id).await,
            }
        }
        Commands::Session { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                SessionCmd::Create(o) => session::create(&c, o, json).await,
                SessionCmd::List => session::list(&c, json).await,
                SessionCmd::Get { id } => session::get(&c, &id, json).await,
                SessionCmd::Exec {
                    id,
                    cmd,
                    timeout_secs,
                } => {
                    // The command line can hold secrets: the record names
                    // the session, never the command.
                    let st = status::Status::new("Command");
                    st.working("Running a command in the session");
                    let r = session::exec(&c, &id, &cmd, timeout_secs, json).await;
                    st.finish(&r, "The command exited 0", "The command failed");
                    r
                }
                SessionCmd::Delete { id } => session::remove(&c, &id).await,
                // Handled before the runtime starts, like `tui`.
                SessionCmd::Shell { .. } => Ok(()),
                SessionCmd::Logs { id, since } => session::logs(&c, &id, since).await,
                SessionCmd::Log {
                    id,
                    limit,
                    from_start,
                } => session::log(&c, &id, limit, !from_start, json).await,
                SessionCmd::Resume { id } => session::resume(&c, &id, json).await,
                SessionCmd::Prompt { id, text } => session::prompt(&c, &id, &text, json).await,
            }
        }
        Commands::Image { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                ImageCmd::List => image::list(&c, json).await,
                ImageCmd::Enable {
                    uri,
                    config,
                    no_wait,
                } => image::enable(&c, &uri, &config, no_wait, json).await,
                ImageCmd::Config { uri } => image::config(&c, &uri).await,
                ImageCmd::Update {
                    uri,
                    config,
                    allow_recapture,
                    no_wait,
                } => image::update(&c, &uri, &config, allow_recapture, no_wait, json).await,
                ImageCmd::Disable { uri } => image::disable(&c, &uri).await,
                ImageCmd::Refresh { uri, recapture } => {
                    image::refresh(&c, &uri, recapture, json).await
                }
                ImageCmd::Jobs => image::jobs(&c, json).await,
                ImageCmd::Job { id } => image::job(&c, &id, json).await,
                ImageCmd::PollJob { id } => image::poll_job_verb(&c, &id, json).await,
                ImageCmd::RetryJob { id, no_wait } => {
                    image::retry_job(&c, &id, no_wait, json).await
                }
            }
        }
        Commands::Registry { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                RegistryCmd::Add(o) => image::registry_add(&c, o, json).await,
                RegistryCmd::List => image::registry_list(&c, json).await,
                RegistryCmd::Rm { host } => image::registry_rm(&c, &host).await,
            }
        }
        Commands::Host { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                HostCmd::List => host::list(&c, json).await,
                HostCmd::Get { id } => host::get(&c, &id, json).await,
                HostCmd::Drain { id } => host::drain(&c, &id).await,
                HostCmd::Delete { id } => host::remove(&c, &id).await,
                HostCmd::Uncordon { id } => host::uncordon(&c, &id, json).await,
                HostCmd::Evacuate { session_id } => host::evacuate(&c, &session_id, json).await,
            }
        }
        Commands::Profile { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                ProfileCmd::List => profile::list(&c, json).await,
                ProfileCmd::Get { id } => profile::get(&c, &id, json).await,
            }
        }
        Commands::Apikey { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                ApikeyCmd::Create {
                    name,
                    role,
                    expires_at,
                } => apikey::create(&c, &name, &role, &expires_at, json).await,
                ApikeyCmd::List => apikey::list(&c, json).await,
                ApikeyCmd::Revoke { id } => apikey::revoke(&c, &id, json).await,
            }
        }
        Commands::Admin { cmd } => {
            let c = ctx.clients()?;
            match cmd {
                AdminCmd::Flush { session_id } => admin::flush(&c, &session_id, json).await,
                AdminCmd::EvictIdle { session_id } => {
                    admin::evict_idle(&c, &session_id, json).await
                }
                AdminCmd::Gc { apply, grace_secs } => admin::gc(&c, apply, grace_secs, json).await,
            }
        }
        // Dispatched in `main` before the runtime starts.
        Commands::Tui { .. } => Ok(()),
    }
}

fn main() {
    // Usage errors: commander exits 1 (clap's default is 2). `--help` and
    // `--version` are not errors.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { 1 } else { 0 };
            let _ = e.print();
            std::process::exit(code);
        }
    };
    if cli.version {
        println!("{BUILD_VERSION}");
        return;
    }
    // The shell owns the terminal and its own runtime, like the TUI.
    if let Some(Commands::Session {
        cmd: SessionCmd::Shell { id },
    }) = &cli.command
    {
        let ctx = Ctx {
            host: resolve_host(cli.url.as_deref()),
            json: false,
        };
        let clients = match ctx.clients() {
            Ok(c) => c,
            Err(CliError::Message(m)) => {
                eprintln!("engrams: {m}");
                std::process::exit(1);
            }
            Err(CliError::Exit(code)) => std::process::exit(code),
        };
        if let Err(m) = engram_tui::shell_passthrough(clients, id) {
            eprintln!("engrams: {m}");
            std::process::exit(1);
        }
        return;
    }
    // The TUI owns its own runtime and the terminal; it never runs inside
    // the CLI's runtime below.
    if let Some(Commands::Tui {
        session_id,
        no_alt_screen,
        no_mouse,
        ax,
    }) = &cli.command
    {
        let ctx = Ctx {
            host: resolve_host(cli.url.as_deref()),
            json: false,
        };
        let clients = match ctx.clients() {
            Ok(c) => c,
            Err(CliError::Message(m)) => {
                eprintln!("engrams: {m}");
                std::process::exit(1);
            }
            Err(CliError::Exit(code)) => std::process::exit(code),
        };
        let env_on = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
        let ax = *ax || env_on("ENGRAMS_TUI_AX");
        let opts = engram_tui::Options {
            session_id: session_id.clone(),
            inline: ax || *no_alt_screen || env_on("ENGRAMS_TUI_NO_ALT_SCREEN"),
            // Native scrollback mode leaves the mouse to the terminal: the
            // transcript is in its scrollback, where its own selection works.
            mouse: !ax
                && !*no_alt_screen
                && !*no_mouse
                && !env_on("ENGRAMS_TUI_NO_MOUSE")
                && !env_on("ENGRAMS_TUI_NO_ALT_SCREEN"),
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            ax,
        };
        if let Err(m) = engram_tui::run(clients, opts) {
            eprintln!("engrams: {m}");
            std::process::exit(1);
        }
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match rt.block_on(run(cli)) {
        Ok(()) => 0,
        Err(CliError::Message(m)) => {
            eprintln!("engrams: {m}");
            1
        }
        Err(CliError::Exit(code)) => code,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// Every line of help the CLI can print: about texts and argument
    /// help, for every subcommand.
    fn help_texts(cmd: &clap::Command, out: &mut Vec<String>) {
        for s in [cmd.get_about(), cmd.get_long_about()]
            .into_iter()
            .flatten()
        {
            out.push(format!("{}: {s}", cmd.get_name()));
        }
        for a in cmd.get_arguments() {
            for s in [a.get_help(), a.get_long_help()].into_iter().flatten() {
                out.push(format!("{} {}: {s}", cmd.get_name(), a.get_id()));
            }
        }
        for sub in cmd.get_subcommands() {
            help_texts(sub, out);
        }
    }

    /// Help is product surface: it never points at the repository's
    /// design records or internal documents.
    #[test]
    fn help_never_cites_internal_documents() {
        let mut texts = Vec::new();
        help_texts(&super::Cli::command(), &mut texts);
        let bad: Vec<&String> = texts
            .iter()
            .filter(|t| {
                let cites_adr = t.match_indices("ADR").any(|(i, _)| {
                    t[i + 3..]
                        .trim_start_matches([' ', '-'])
                        .starts_with(|c: char| c.is_ascii_digit())
                });
                cites_adr || t.contains("docs/adr") || t.contains("DESIGN.md")
            })
            .collect();
        assert!(bad.is_empty(), "help cites internal documents: {bad:#?}");
    }
}
