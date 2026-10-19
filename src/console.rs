//! Workstation web console: companion resolution, gateway
//! lifecycle, and the loopback console server.

use super::*;

fn console_entry(explicit: Option<PathBuf>) -> Result<PathBuf, dg::error::Error> {
    use dg::error::Error;
    // TRUST BOUNDARY (owner decision 2026-09-27): the last candidate below is
    // resolved relative to the process working directory and handed straight to
    // `node`, so `dg web` run inside an untrusted checkout executes that
    // directory's JavaScript with the operator's privileges. This is the
    // intended repo-local developer workflow and matches any Node-based dev
    // tool, so the fallback is kept deliberately. The two exe-relative
    // candidates are preferred, so an installed dg uses the companion shipped
    // beside it. An operator who needs the stricter posture should pass
    // --entry explicitly.
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path);
        }
        eprintln!("console companion not found: {}", path.display());
        return Err(Error::new(
            400,
            "invalid_console_entry",
            "console companion entrypoint does not exist",
        ));
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("../../web/dist/server.js"));
        candidates.push(dir.join("../web/dist/server.js"));
    }
    candidates.push(PathBuf::from("web/dist/server.js"));
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    for candidate in &candidates {
        eprintln!("console companion not found: {}", candidate.display());
    }
    Err(Error::new(
        400,
        "invalid_console_entry",
        "console companion not found; run `npm run build --prefix web` first",
    ))
}

/// Stop an auto-started gateway during `dg web` shutdown. No child means
/// the operator's own gateway is in use; it is never touched.
async fn stop_gateway(gateway: &mut Option<tokio::process::Child>) {
    let Some(child) = gateway.as_mut() else {
        return;
    };
    #[cfg(unix)]
    terminate_child(child).await;
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
    *gateway = None;
}

/// Install the SIGTERM/SIGINT listeners, or stop both children and fail.
/// This runs before the ready line is printed so no signal can reach this
/// process while the OS default disposition is still in force.
#[cfg(unix)]
async fn install_signals(
    gateway: &mut Option<tokio::process::Child>,
    child: &mut tokio::process::Child,
) -> Result<(tokio::signal::unix::Signal, tokio::signal::unix::Signal), dg::error::Error> {
    use dg::error::Error;
    use tokio::signal::unix::{SignalKind, signal};
    let registered = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    );
    let (Ok(sigterm), Ok(sigint)) = registered else {
        stop_gateway(gateway).await;
        terminate_child(child).await;
        let _ = child.wait().await;
        return Err(Error::internal());
    };
    // Both streams are polled by the select! in the caller; returning them
    // keeps the registrations alive for the supervised lifetime.
    Ok((sigterm, sigint))
}

/// Ask the companion to stop gracefully (it closes its listener and any
/// managed SSH tunnel), escalating only if it ignores SIGTERM.
#[cfg(unix)]
async fn terminate_child(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        // Best effort: the child may already be gone.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// Render a `host:port` authority for a socket address, bracketing an IPv6
/// literal. A bare `::1:8787` does not parse as a `SocketAddr` and is not a
/// valid URL authority either; `[::1]:8787` is both. A host that already
/// carries brackets (as `Url::host_str` returns for IPv6) is left alone.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Loopback-only check mirroring the companion bind gate (web/src/server.ts
/// isLoopback): any 127/8 address, the IPv6 loopback (bracketed or bare),
/// or localhost. Anything else must not become a bind address, probe target,
/// or printed URL.
fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim().to_lowercase();
    if normalized == "localhost" || normalized == "::1" || normalized == "[::1]" {
        return true;
    }
    let parts: Vec<&str> = normalized.split('.').collect();
    parts.len() == 4
        && parts[0] == "127"
        && parts[1..].iter().all(|octet| octet.parse::<u8>().is_ok())
}

/// Loopback-http gateway this process may auto-start a server on: the
/// `host:port` to bind and probe, plus the origin the companion should use.
/// Anything else (https, non-loopback, unparsable) is left to the companion
/// to report; this process never spawns remote listeners.
///
/// `localhost` is accepted the way `is_loopback_host` accepts it, but is
/// bound as its `127.0.0.1` literal: the auto-start needs an address this
/// process can bind and connect, and a hostname may resolve to `::1`, which
/// a 127.0.0.1 listener never answers.
struct GatewayTarget {
    /// `host:port` to bind and probe.
    bind: String,
    /// Origin the companion talks to: the original when it was already a
    /// literal, otherwise the `127.0.0.1` form actually bound.
    origin: String,
}

fn gateway_target(server: &str) -> Option<GatewayTarget> {
    let url = reqwest::Url::parse(server).ok()?;
    if url.scheme() != "http" || url.path() != "/" {
        return None;
    }
    let host = url.host_str()?;
    // An unspecified port is not a target this process may bind: the implied
    // :80 fails EACCES for any non-root operator, and the child then dies
    // behind a message about diagnosing rather than about the port.
    let port = url.port()?;
    let bind = if host
        .trim_matches(['[', ']'])
        .eq_ignore_ascii_case("localhost")
    {
        "127.0.0.1".to_string()
    } else {
        let ip: std::net::IpAddr = host.trim_matches(['[', ']']).parse().ok()?;
        if !ip.is_loopback() {
            return None;
        }
        host.to_string()
    };
    let origin = if bind == host {
        server.to_string()
    } else {
        format!("http://127.0.0.1:{port}")
    };
    Some(GatewayTarget {
        bind: authority(&bind, port),
        origin,
    })
}

/// Wait until `target` accepts TCP connections, the child exits, or the
/// deadline passes. An accepted connection means the service bound and is
/// past credential loading; details go to stderr, codes stay static.
async fn wait_for_listener(
    target: &str,
    child: &mut tokio::process::Child,
    timeout: Duration,
    service: &str,
    code: &'static str,
    exit_message: &'static str,
    timeout_message: &'static str,
) -> Result<(), dg::error::Error> {
    use dg::error::Error;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(target).await.is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().map_err(|_| Error::internal())? {
            eprintln!("{service} exited before becoming ready: {status}");
            return Err(Error::new(400, code, exit_message));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(Error::new(400, code, timeout_message));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Auto-serve leg of `dg web`: when the loopback gateway is unreachable,
/// start `dg serve` on it (this same executable) and wait for readiness.
/// Returns the supervised child plus the origin the companion must use,
/// which is `Some` only when a hostname server was rewritten to the literal
/// this process bound. No child and no rewrite mean a gateway is already
/// up or the target is not auto-startable.
async fn ensure_gateway(
    server: &str,
    data_dir: &Path,
) -> Result<(Option<tokio::process::Child>, Option<String>), dg::error::Error> {
    use dg::error::Error;
    let Some(target) = gateway_target(server) else {
        return Ok((None, None));
    };
    if tokio::net::TcpStream::connect(&target.bind).await.is_ok() {
        return Ok((None, None));
    }
    let exe = std::env::current_exe().map_err(|_| Error::internal())?;
    let mut child = tokio::process::Command::new(exe)
        .args(["serve", "--listen", &target.bind, "--data-dir"])
        .arg(data_dir)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|_| Error::internal())?;
    wait_for_listener(
        &target.bind,
        &mut child,
        Duration::from_secs(15),
        "auto-started gateway",
        "gateway_startup_failed",
        "local gateway exited before becoming ready; run `dg serve` manually to diagnose",
        "local gateway did not become ready in time",
    )
    .await?;
    eprintln!(
        "dg web: no gateway at {server}; started local gateway on {} (data dir {})",
        target.bind,
        data_dir.display()
    );
    let origin = (target.origin != server).then_some(target.origin);
    Ok((Some(child), origin))
}

/// Start the workstation console companion, print its loopback URL once
/// it accepts connections, and supervise it until it exits or we are
/// signalled. Only the key *path* is forwarded; key material is never
/// read here. Remaining environment (notably DG_SSH_*) passes through
/// untouched so managed tunnels keep working.
pub(super) async fn run_console(
    server: &str,
    key_path: Option<PathBuf>,
    port: u16,
    entry: Option<PathBuf>,
    data_dir: PathBuf,
) -> Result<(), dg::error::Error> {
    use dg::error::Error;
    if !(1024..=65535).contains(&port) {
        return Err(Error::new(
            400,
            "invalid_console_port",
            "console port must be 1024-65535",
        ));
    }
    let key_path = key_path.ok_or(Error::new(400, "missing_credential", "--key-file required"))?;
    let console_host = std::env::var("DG_CONSOLE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    // The value becomes the companion bind address, the readiness-probe
    // target, and the printed URL. Refuse anything the companion would not
    // bind (server.ts isLoopback) here, before a hostname can turn the
    // probe into a TCP connection to an arbitrary host.
    if !is_loopback_host(&console_host) {
        return Err(Error::new(
            400,
            "invalid_console_host",
            "DG_CONSOLE_HOST must be a loopback address",
        ));
    }
    let entry = console_entry(entry)?;
    // `gateway_origin` is Some only when a hostname --server was rewritten
    // to the literal this process bound: the companion then talks to
    // exactly the auto-started listener instead of racing `localhost`'s
    // possible ::1 answer.
    let (mut gateway, gateway_origin) = ensure_gateway(server, &data_dir).await?;
    let server = gateway_origin.as_deref().unwrap_or(server);
    // Past this point every exit path must stop the auto-started gateway:
    // tokio's Child has no kill_on_drop, so an early `?` orphans a `dg serve`
    // that keeps holding the data dir and the port. This is most reachable
    // when node is missing, which fails the spawn below.
    let mut command = tokio::process::Command::new("node");
    command
        .arg(&entry)
        .env("DG_SERVER", server)
        .env("DG_KEY_FILE", &key_path)
        .env("DG_CONSOLE_PORT", port.to_string())
        .stdin(std::process::Stdio::null());
    if std::env::var_os("DG_CONSOLE_HOST").is_none() {
        command.env("DG_CONSOLE_HOST", &console_host);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            stop_gateway(&mut gateway).await;
            return Err(if error.kind() == std::io::ErrorKind::NotFound {
                Error::new(
                    400,
                    "console_runtime_unavailable",
                    "node executable not found on PATH; the console companion requires node",
                )
            } else {
                Error::internal()
            });
        }
    };
    // The companion only binds after loading the credential and starting
    // any managed tunnel, so an accepted connection means ready. Probe the
    // configured host: a fixed 127.0.0.1 never reaches a ::1 listener.
    let address = authority(&console_host, port);
    if let Err(error) = wait_for_listener(
        &address,
        &mut child,
        Duration::from_secs(30),
        "console companion",
        "console_startup_failed",
        "console companion exited before becoming ready; see output above",
        "console companion did not become ready in time",
    )
    .await
    {
        stop_gateway(&mut gateway).await;
        terminate_child(&mut child).await;
        let _ = child.wait().await;
        return Err(error);
    }
    // Listeners before the ready line. A SIGTERM/SIGINT arriving between
    // the companion accepting connections and the select! below used to kill
    // this process by default disposition -- which both leaves an
    // auto-started `dg serve` orphaned (the class 8e011e6 closed on the error
    // paths) and makes a supervised shutdown look like a crash. tokio buffers
    // a signal delivered after registration, so the select! below still sees
    // one that lands during the print.
    #[cfg(unix)]
    let signals = install_signals(&mut gateway, &mut child).await?;
    println!("dg web ready: open http://{address} in a browser");
    if std::io::stdout().flush().is_err() {
        stop_gateway(&mut gateway).await;
        terminate_child(&mut child).await;
        let _ = child.wait().await;
        return Err(Error::internal());
    }
    #[cfg(unix)]
    {
        let (mut sigterm, mut sigint) = signals;
        let gateway_exited = async {
            match gateway.as_mut() {
                Some(child) => {
                    let _ = child.wait().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            status = child.wait() => {
                stop_gateway(&mut gateway).await;
                let status = status.map_err(|_| Error::internal())?;
                if status.success() {
                    Ok(())
                } else {
                    eprintln!("console companion exited: {status}");
                    Err(Error::new(
                        500,
                        "console_companion_failed",
                        "console companion exited with an error",
                    ))
                }
            }
            // Shutdown was operator-initiated; the companion's
            // signal-induced exit status is still a clean stop.
            _ = sigterm.recv() => {
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                stop_gateway(&mut gateway).await;
                Ok(())
            }
            _ = sigint.recv() => {
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                stop_gateway(&mut gateway).await;
                Ok(())
            }
            // Our gateway died under us; the console is useless without
            // it, so stop the companion instead of serving errors.
            _ = gateway_exited => {
                eprintln!("auto-started gateway exited; stopping console");
                terminate_child(&mut child).await;
                let _ = child.wait().await;
                Err(Error::new(
                    500,
                    "gateway_exited",
                    "auto-started gateway exited; console stopped",
                ))
            }
        }
    }
    #[cfg(not(unix))]
    {
        let status = child.wait().await.map_err(|_| Error::internal())?;
        stop_gateway(&mut gateway).await;
        if status.success() {
            Ok(())
        } else {
            eprintln!("console companion exited: {status}");
            Err(Error::new(
                500,
                "console_companion_failed",
                "console companion exited with an error",
            ))
        }
    }
}
