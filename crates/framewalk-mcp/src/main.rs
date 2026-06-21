//! `framewalk-mcp` — MCP server binary for driving GDB via the framewalk library.
//!
//! Spawned over stdio by Claude Desktop / Claude Code / other
//! MCP-speaking agents, this binary:
//!
//! 1. Parses its own CLI args with clap.
//! 2. Initialises `tracing-subscriber` to write to **stderr only** —
//!    stdout is reserved for MCP protocol traffic on the stdio transport.
//! 3. Spawns a GDB child via [`framewalk_mi_transport::spawn`].
//! 4. Hands the transport handle to a [`FramewalkMcp`] rmcp server and
//!    runs it over the stdio transport until the MCP client disconnects.
//! 5. Gracefully shuts down the GDB child and exits.

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use framewalk_mcp::{BackgroundTasks, Config, FramewalkMcp, SchemeHandle, SchemeSettings};
use framewalk_mi_transport::{GdbConfig, SshConfig, spawn};
use rmcp::{ServiceExt, transport::stdio};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Translate the parsed CLI [`Config`] into a transport [`GdbConfig`].
///
/// Applies `--core` / `--pid` / `--gdb-arg` as GDB extra args, and wraps
/// the invocation in SSH when `--ssh-host` is set. The caller is
/// responsible for validating that `--core` and `--pid` are not both set.
fn build_gdb_config(config: &Config) -> GdbConfig {
    // Build GDB extra args from --core, --pid, and --gdb-arg flags.
    let mut gdb_extra_args: Vec<String> = Vec::new();
    if let Some(ref core_path) = config.core {
        gdb_extra_args.push("--core".to_string());
        gdb_extra_args.push(core_path.to_string_lossy().into_owned());
    }
    if let Some(pid) = config.pid {
        gdb_extra_args.push("-p".to_string());
        gdb_extra_args.push(pid.to_string());
    }
    gdb_extra_args.extend(config.gdb_args.clone());

    let mut gdb_config = GdbConfig::new()
        .with_program(&config.gdb)
        .with_non_stop(config.non_stop)
        .with_args(&gdb_extra_args);
    if let Some(cwd) = &config.cwd {
        gdb_config = gdb_config.with_cwd(cwd.clone());
    }
    if let Some(ref host) = config.ssh_host {
        let mut ssh = SshConfig::new(host).with_port(config.ssh_port);
        if let Some(ref user) = config.ssh_user {
            ssh = ssh.with_user(user);
        }
        if let Some(ref key) = config.ssh_identity_file {
            ssh = ssh.with_identity_file(key.clone());
        }
        gdb_config = gdb_config.with_ssh(ssh);
    }
    gdb_config
}

/// `current_thread` is load-bearing: rmcp dispatches each MCP request
/// as an independent `tokio::spawn` task, so on a multi-thread runtime
/// two concurrent `scheme_eval` tool calls can be polled in arbitrary
/// order, causing out-of-order processing of user Scheme code.  A
/// single-threaded runtime polls spawned tasks in FIFO order from the
/// ready queue, which preserves the request-arrival ordering that a
/// mutable-state scripting tool depends on.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();

    // Stderr-only logging: stdout is reserved for MCP protocol traffic.
    // The env filter lets operators tune verbosity via `--log` or
    // `FRAMEWALK_LOG`.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&config.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    info!(
        allow_shell = config.allow_shell,
        non_stop = config.non_stop,
        gdb = %config.gdb,
        ?config.mode,
        scheme_eval_timeout_secs = config.scheme_eval_timeout_secs,
        wait_for_stop_timeout_secs = config.wait_for_stop_timeout_secs,
        ssh_host = ?config.ssh_host,
        core = ?config.core,
        pid = config.pid,
        "starting framewalk-mcp"
    );

    // Validate mutual exclusivity of --core and --pid.
    if config.core.is_some() && config.pid.is_some() {
        anyhow::bail!("--core and --pid are mutually exclusive; specify only one");
    }

    let gdb_config = build_gdb_config(&config);
    let transport = spawn(gdb_config)
        .await
        .context("failed to spawn gdb subprocess")?;
    let transport = Arc::new(transport);
    let background_tasks = Arc::new(BackgroundTasks::default());

    let scheme_settings = SchemeSettings {
        eval_timeout: std::time::Duration::from_secs(config.scheme_eval_timeout_secs),
        wait_timeout: std::time::Duration::from_secs(config.wait_for_stop_timeout_secs),
    };

    // Spawn the Steel Scheme worker thread.  The worker is alive in
    // both modes — `scheme_eval` is always registered.  Prelude
    // load failure is a hard startup error: if the scripting surface
    // we advertise is broken we'd rather fail fast than boot into a
    // state where every `(gdb-version)` call raises FreeIdentifier.
    let rt = tokio::runtime::Handle::current();
    let scheme = SchemeHandle::spawn(
        Arc::clone(&transport),
        config.allow_shell,
        rt,
        scheme_settings,
    )
    .await
    .context("failed to initialise scheme worker")?;
    let scheme = Arc::new(scheme);

    // Build and run the MCP server over stdio.  `FramewalkMcp::new`
    // clones both Arcs into the server struct; we retain our own
    // local `scheme` binding so we can join the worker thread on
    // shutdown below.
    let server = FramewalkMcp::new(
        Arc::clone(&transport),
        Arc::clone(&background_tasks),
        config.allow_shell,
        config.mode,
        Arc::clone(&scheme),
    );
    let service = server
        .serve(stdio())
        .await
        .context("failed to start MCP stdio transport")?;

    info!("framewalk-mcp serving on stdio");

    // `waiting()` consumes `service` and returns when the MCP client
    // disconnects.  Because the call takes `service` by value, the
    // rmcp service wrapper — and with it the `FramewalkMcp` struct,
    // its tool router, and the `Arc<TransportHandle>` /
    // `Arc<SchemeHandle>` clones it held — is dropped as soon as
    // this future resolves.
    service
        .waiting()
        .await
        .context("MCP service loop exited with error")?;

    info!("mcp client disconnected; shutting down gdb");

    // Ordered shutdown.  After `waiting()` has returned the service
    // no longer accepts new work, so first drain best-effort tasks that
    // may have cloned the transport.  Then the only live
    // `Arc<SchemeHandle>` is our local binding, and the only live
    // `Arc<TransportHandle>` clones are the local binding plus the one
    // captured by the scheme worker thread's closure.
    //
    //   1. Abort and join registered background tasks, dropping their
    //      transport Arcs.
    //   2. Consume the scheme Arc, close the eval channel, and join
    //      the worker thread.  `JoinHandle::join` is blocking so we
    //      offload it to `spawn_blocking` rather than stalling the
    //      runtime thread.  Thread exit drops the worker's captured
    //      transport Arc.
    //   3. `Arc::into_inner(transport)` is now the unique holder and
    //      returns `Some`; call `shutdown()` to issue `-gdb-exit`
    //      and collect the child's exit status.
    background_tasks.abort_and_wait().await;

    let scheme_handle =
        Arc::into_inner(scheme).expect("scheme Arc must be unique after rmcp service drop");
    if let Some(thread) = scheme_handle.join() {
        tokio::task::spawn_blocking(move || match thread.join() {
            Ok(()) => {}
            Err(panic) => {
                tracing::warn!(?panic, "scheme worker thread panicked during shutdown");
            }
        })
        .await
        .context("failed to join scheme worker thread")?;
    }

    let gdb = Arc::into_inner(transport)
        .expect("transport Arc must be unique after service + scheme join");
    if let Err(err) = gdb.shutdown().await {
        tracing::warn!(%err, "gdb shutdown returned an error");
    }

    Ok(())
}
