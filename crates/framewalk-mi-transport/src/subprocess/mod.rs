//! The [`spawn`] entry point: launch GDB as a subprocess and return a
//! [`TransportHandle`](crate::handle::TransportHandle) bound to its stdio.

pub(crate) mod config;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use framewalk_mi_protocol::{CommandOutcome, Connection};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::error::TransportError;
use crate::handle::TransportHandle;
use crate::pump::{run_reader, run_stderr_logger, run_writer};
use crate::shared::SharedState;

pub use config::GdbConfig;
pub use config::SshConfig;

/// Capacity of the outbound byte-buffer channel between [`TransportHandle::submit`]
/// and the writer task. 64 pending command buffers is plenty for any
/// realistic frontend; each buffer holds one complete encoded command.
const WRITE_CHANNEL_CAPACITY: usize = 64;

/// Session bootstrap commands that must succeed before framewalk serves
/// requests. These shape the MI semantics the rest of the stack relies on:
/// async command completion and no interactive pager or confirmation prompts.
/// Non-stop mode is conditional — see [`GdbConfig::non_stop`].
const BOOTSTRAP_COMMANDS: &[&str] = &[
    "-gdb-set mi-async on",
    "-gdb-set pagination off",
    "-gdb-set confirm off",
];

/// Build the argv vector for the GDB subprocess.
///
/// When `config.ssh` is set, the GDB invocation is wrapped in an SSH
/// command so GDB runs on a remote host. Otherwise GDB is launched
/// directly. The returned vector is the complete argv (program first);
/// `build_command` feeds it to a `tokio::process::Command`.
///
/// Extracted as a pure function so the SSH command construction — the
/// load-bearing part of remote-GDB support — is unit-testable without
/// spawning a process.
fn build_argv(config: &GdbConfig) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mi_arg = format!("--interpreter={}", config.mi_version.as_interpreter_arg());
    let mut gdb_argv: Vec<OsString> = vec![
        config.program.clone().into(),
        mi_arg.into(),
        "--quiet".into(),
        "--nx".into(),
    ];
    gdb_argv.extend(config.extra_args.iter().map(OsString::from));

    if let Some(ssh) = &config.ssh {
        let mut argv: Vec<OsString> = Vec::with_capacity(gdb_argv.len() + 8);
        argv.push("ssh".into());
        if ssh.port != 22 {
            argv.push("-p".into());
            argv.push(ssh.port.to_string().into());
        }
        if let Some(ref key) = ssh.identity_file {
            argv.push("-i".into());
            argv.push(key.clone().into_os_string());
        }
        // Disable pseudo-terminal allocation — we drive GDB through MI
        // over stdio and must not let SSH interpose a TTY.
        argv.push("-T".into());
        // Construct the remote user@host target.
        let target = if let Some(ref user) = ssh.user {
            format!("{}@{}", user, ssh.host)
        } else {
            ssh.host.clone()
        };
        argv.push(target.into());
        // `--` separates SSH options from the remote command.
        argv.push("--".into());
        argv.extend(gdb_argv);
        argv
    } else {
        gdb_argv
    }
}

/// Build the `tokio::process::Command` for the GDB subprocess.
///
/// Delegates argv construction to [`build_argv`] and sets the program
/// to the first element. The returned command does **not** yet have
/// stdio pipes or `kill_on_drop` set — the caller applies those.
fn build_command(config: &GdbConfig) -> Command {
    let mut argv = build_argv(config);
    // The first element is the program (either "ssh" or the GDB binary).
    let program = argv.remove(0);
    let mut cmd = Command::new(program);
    cmd.args(argv);
    cmd
}

/// Spawn a GDB subprocess with the given [`GdbConfig`] and return an
/// async [`TransportHandle`] ready to submit commands.
///
/// When [`GdbConfig::ssh`] is set, the GDB command is wrapped in an SSH
/// invocation so GDB runs on a remote host. All MI traffic flows
/// transparently through the SSH stdio pipe.
///
/// The child process is launched with `kill_on_drop` enabled so it
/// cannot outlive the handle in an error path. Its stdio is piped into
/// three background tokio tasks (reader, writer, stderr logger) that
/// handle the full byte pipeline. The caller only interacts with the
/// returned handle.
///
/// # Errors
///
/// Returns [`TransportError::Spawn`] if the child cannot be started
/// (e.g. `gdb` not on `PATH`, or SSH connection failed) or
/// [`TransportError::PipeMissing`] if tokio fails to attach a stdio
/// pipe for some reason.
pub async fn spawn(config: GdbConfig) -> Result<TransportHandle, TransportError> {
    let is_ssh = config.ssh.is_some();

    if is_ssh {
        info!(
            host = %config.ssh.as_ref().unwrap().host,
            port = config.ssh.as_ref().unwrap().port,
            program = %config.program,
            mi_version = ?config.mi_version,
            extra_args = ?config.extra_args,
            "spawning gdb subprocess via ssh"
        );
    } else {
        info!(
            program = %config.program,
            mi_version = ?config.mi_version,
            extra_args = ?config.extra_args,
            "spawning gdb subprocess"
        );
    }

    let mut cmd = build_command(&config);

    // NOTE on SSH semantics: `env` and `cwd` here are applied to the
    // *local* process being spawned — which is `ssh` itself when an SSH
    // config is set, not the remote GDB. SSH does not forward arbitrary
    // environment variables to the remote host by default, so variables
    // intended for the remote GDB (e.g. `LD_LIBRARY_PATH`) will NOT
    // reach it through this path. Callers that need remote env should
    // embed it in the remote command (e.g. via `extra_args` prefix) or
    // configure `SendEnv`/`AcceptEnv` on the SSH client/server. `cwd`
    // is harmless under SSH (ssh ignores it for the remote command).
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &config.cwd {
        cmd.current_dir(cwd);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        if is_ssh {
            TransportError::Spawn(std::io::Error::new(
                e.kind(),
                format!("SSH spawn failed (is `ssh` on PATH and the remote host reachable?): {e}"),
            ))
        } else {
            TransportError::Spawn(e)
        }
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or(TransportError::PipeMissing("stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(TransportError::PipeMissing("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(TransportError::PipeMissing("stderr"))?;

    debug!(pid = ?child.id(), "gdb spawned");

    // Build shared state and the write channel.
    let shared = Arc::new(SharedState::new(Connection::new()));
    let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(WRITE_CHANNEL_CAPACITY);

    // Spawn the three background tasks and retain their JoinHandles so
    // `TransportHandle::shutdown` can observe task termination
    // deterministically instead of leaving them detached.
    let reader_task = tokio::spawn({
        let shared = Arc::clone(&shared);
        async move {
            if let Err(err) = run_reader(shared, stdout).await {
                tracing::error!(%err, "reader task exited with error");
            }
        }
    });
    let writer_task = tokio::spawn(async move {
        if let Err(err) = run_writer(stdin, write_rx).await {
            tracing::error!(%err, "writer task exited with error");
        }
    });
    let stderr_task = tokio::spawn(async move {
        if let Err(err) = run_stderr_logger(stderr).await {
            tracing::error!(%err, "stderr logger task exited with error");
        }
    });

    let handle = TransportHandle {
        shared,
        write_tx,
        child,
        reader_task,
        writer_task,
        stderr_task,
    };

    bootstrap_session(&handle, config.non_stop).await?;

    Ok(handle)
}

async fn bootstrap_session(handle: &TransportHandle, non_stop: bool) -> Result<(), TransportError> {
    let non_stop_cmd = "-gdb-set non-stop on";
    let commands: Vec<&str> = if non_stop {
        let mut cmds: Vec<&str> = BOOTSTRAP_COMMANDS.to_vec();
        cmds.insert(1, non_stop_cmd);
        cmds
    } else {
        BOOTSTRAP_COMMANDS.to_vec()
    };

    for raw in &commands {
        let Ok(outcome) =
            tokio::time::timeout(Duration::from_secs(120), handle.submit_raw(raw)).await
        else {
            return Err(TransportError::Bootstrap {
                command: (*raw).to_string(),
                message: "timed out waiting for bootstrap reply".to_string(),
            });
        };

        match outcome {
            Ok(CommandOutcome::Done(_) | CommandOutcome::Connected(_)) => {}
            Ok(CommandOutcome::Error { msg, .. }) => {
                return Err(TransportError::Bootstrap {
                    command: (*raw).to_string(),
                    message: msg,
                });
            }
            Ok(other) => {
                return Err(TransportError::Bootstrap {
                    command: (*raw).to_string(),
                    message: format!("unexpected outcome: {other:?}"),
                });
            }
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::config::SshConfig;
    use framewalk_mi_protocol::MiVersion;
    use std::ffi::OsString;

    fn argv_strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_argv_local_is_bare_gdb_invocation() {
        let config = GdbConfig::new().with_program("gdb");
        let argv = build_argv(&config);
        assert_eq!(
            argv_strings(&argv),
            vec!["gdb", "--interpreter=mi3", "--quiet", "--nx"]
        );
    }

    #[test]
    fn build_argv_local_forwards_extra_args_in_order() {
        let config = GdbConfig::new()
            .with_program("gdb")
            .with_arg("--core")
            .with_arg("/tmp/core");
        let argv = build_argv(&config);
        assert_eq!(
            argv_strings(&argv),
            vec![
                "gdb",
                "--interpreter=mi3",
                "--quiet",
                "--nx",
                "--core",
                "/tmp/core"
            ]
        );
    }

    #[test]
    fn build_argv_ssh_wraps_gdb_with_defaults() {
        let config = GdbConfig::new()
            .with_program("gdb")
            .with_ssh(SshConfig::new("remote.example.com"));
        let argv = build_argv(&config);
        // No -p (port 22), no -i, -T, bare host, --, then GDB argv.
        assert_eq!(
            argv_strings(&argv),
            vec![
                "ssh",
                "-T",
                "remote.example.com",
                "--",
                "gdb",
                "--interpreter=mi3",
                "--quiet",
                "--nx"
            ]
        );
    }

    #[test]
    fn build_argv_ssh_includes_port_user_and_identity() {
        let config = GdbConfig::new().with_program("/opt/gdb").with_ssh(
            SshConfig::new("10.0.0.1")
                .with_port(2222)
                .with_user("admin")
                .with_identity_file("/home/me/.ssh/id_ed25519"),
        );
        let argv = build_argv(&config);
        assert_eq!(
            argv_strings(&argv),
            vec![
                "ssh",
                "-p",
                "2222",
                "-i",
                "/home/me/.ssh/id_ed25519",
                "-T",
                "admin@10.0.0.1",
                "--",
                "/opt/gdb",
                "--interpreter=mi3",
                "--quiet",
                "--nx"
            ]
        );
    }

    #[test]
    fn build_argv_ssh_forwards_gdb_extra_args_after_separator() {
        let config = GdbConfig::new()
            .with_program("gdb")
            .with_arg("--core")
            .with_arg("/var/core/dump")
            .with_ssh(SshConfig::new("host").with_port(2222));
        let argv = build_argv(&config);
        assert_eq!(
            argv_strings(&argv),
            vec![
                "ssh",
                "-p",
                "2222",
                "-T",
                "host",
                "--",
                "gdb",
                "--interpreter=mi3",
                "--quiet",
                "--nx",
                "--core",
                "/var/core/dump"
            ]
        );
    }

    #[test]
    fn build_argv_ssh_omits_port_flag_when_default_22() {
        // Regression guard: port 22 must not emit `-p 22`.
        let config = GdbConfig::new()
            .with_program("gdb")
            .with_ssh(SshConfig::new("host"));
        let argv = build_argv(&config);
        let strs = argv_strings(&argv);
        assert!(!strs.contains(&"-p".to_string()));
        assert!(!strs.contains(&"22".to_string()));
    }

    #[test]
    fn build_argv_respects_mi_version() {
        let config = GdbConfig::new()
            .with_program("gdb")
            .with_mi_version(MiVersion::Mi2);
        let argv = build_argv(&config);
        assert!(argv_strings(&argv).contains(&"--interpreter=mi2".to_string()));
    }
}
