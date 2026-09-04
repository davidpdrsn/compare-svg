use std::{
    fmt::Write as _,
    fs,
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{State, WebSocketUpgrade, ws::WebSocket},
    http::{StatusCode, header},
    response::Html,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Args, Parser, Subcommand};
use tokio::{net::TcpListener, sync::watch};
use tracing_subscriber::EnvFilter;

const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 30;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Compare SVGs in the working tree with their versions at Git HEAD",
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Run as if compare-svg was started in this directory
    #[arg(short = 'C', global = true, value_name = "PATH")]
    directory: Option<PathBuf>,

    /// Paths to SVGs in the same Git working tree
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the comparison web server in the foreground
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Seconds to wait without browser activity before shutting down
    #[arg(long, default_value_t = DEFAULT_SHUTDOWN_TIMEOUT_SECONDS)]
    timeout: u64,

    /// Paths to SVGs in the same Git working tree
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Debug)]
struct SnapshotVersions {
    repository_relative_path: PathBuf,
    previous: Option<Vec<u8>>,
    current: Vec<u8>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Some(directory) = &cli.directory {
        std::env::set_current_dir(directory).with_context(|| {
            format!(
                "failed to change working directory to '{}'",
                directory.display()
            )
        })?;
    }

    match cli.command {
        Some(Commands::Serve(args)) => run_server(args),
        None => launch_background_server(&cli.paths),
    }
}

fn launch_background_server(paths: &[PathBuf]) -> Result<()> {
    let executable =
        std::env::current_exe().context("failed to find the compare-svg executable")?;
    let mut child = Command::new(executable)
        .arg("serve")
        .arg("--")
        .args(paths)
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start the comparison server")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture the comparison server URL")?;
    let mut reader = BufReader::new(stdout);
    let mut url = String::new();
    reader
        .read_line(&mut url)
        .context("failed to read the comparison server URL")?;
    let url = url.trim();

    if url.is_empty() {
        let status = child
            .wait()
            .context("failed to wait for the comparison server")?;
        let mut stderr = String::new();
        if let Some(mut child_stderr) = child.stderr.take() {
            child_stderr
                .read_to_string(&mut stderr)
                .context("failed to read the comparison server error")?;
        }
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("comparison server exited before starting ({status})");
        }
        bail!("comparison server exited before starting ({status}): {stderr}");
    }

    drop(child.stderr.take());
    if let Err(error) = open::that(url) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error)
            .with_context(|| format!("failed to open '{url}' in the default browser"));
    }

    Ok(())
}

fn run_server(args: ServeArgs) -> Result<()> {
    init_logging()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create the async runtime")?;

    runtime.block_on(serve(args))
}

fn init_logging() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("compare_svg=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BrowserActivity {
    connections: usize,
    heartbeat: u64,
}

#[derive(Clone)]
struct ServerState {
    html: Arc<String>,
    activity: watch::Sender<BrowserActivity>,
    shutdown_timeout: Duration,
}

async fn serve(args: ServeArgs) -> Result<()> {
    let versions = load_versions(&args.paths)?;
    let html = Arc::new(render_html(&versions));
    let shutdown_timeout = Duration::from_secs(args.timeout);
    let (activity, activity_updates) = watch::channel(BrowserActivity::default());
    let state = ServerState {
        html,
        activity,
        shutdown_timeout,
    };
    let app = Router::new()
        .route("/", get(serve_comparison))
        .route("/lifecycle.js", get(serve_lifecycle_script))
        .route("/heartbeat", post(record_heartbeat))
        .route("/ws", get(upgrade_websocket))
        .with_state(state);
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .context("failed to bind the comparison server")?;
    let address = listener
        .local_addr()
        .context("failed to determine the comparison server address")?;
    let url = format!("http://{address}/");

    tracing::info!(%url, timeout_seconds = args.timeout, "comparison server started");
    println!("{url}");
    std::io::stdout()
        .flush()
        .context("failed to print the comparison server URL")?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(activity_updates, shutdown_timeout))
        .await
        .context("comparison server failed")?;
    tracing::info!("comparison server stopped");

    Ok(())
}

async fn serve_comparison(State(state): State<ServerState>) -> Html<String> {
    Html(state.html.as_ref().clone())
}

async fn serve_lifecycle_script(
    State(state): State<ServerState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    let timeout_millis = state.shutdown_timeout.as_millis();
    let heartbeat_interval_millis = (timeout_millis / 3).clamp(250, 5_000);
    let script = format!(
        r#"(() => {{
  "use strict";

  const heartbeat = () => {{
    fetch("/heartbeat", {{ method: "POST", cache: "no-store" }}).catch(() => {{}});
  }};
  heartbeat();
  window.setInterval(heartbeat, {heartbeat_interval_millis});

  const connect = () => {{
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const socket = new WebSocket(`${{protocol}}//${{window.location.host}}/ws`);
    socket.addEventListener("close", () => window.setTimeout(connect, 1_000));
  }};
  connect();
}})();
"#
    );

    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        script,
    )
}

async fn record_heartbeat(State(state): State<ServerState>) -> StatusCode {
    state.activity.send_modify(|activity| {
        activity.heartbeat = activity.heartbeat.wrapping_add(1);
    });
    tracing::debug!("browser heartbeat received");
    StatusCode::NO_CONTENT
}

async fn upgrade_websocket(
    websocket: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> impl axum::response::IntoResponse {
    tracing::info!("browser WebSocket upgrade requested");
    websocket.on_upgrade(move |socket| track_browser_connection(socket, state))
}

async fn track_browser_connection(mut socket: WebSocket, state: ServerState) {
    state
        .activity
        .send_modify(|activity| activity.connections += 1);
    tracing::info!(
        connections = state.activity.borrow().connections,
        "browser connected"
    );

    while let Some(message) = socket.recv().await {
        if let Err(error) = message {
            tracing::debug!(%error, "browser WebSocket closed with an error");
            break;
        }
    }

    state
        .activity
        .send_modify(|activity| activity.connections -= 1);
    tracing::info!(
        connections = state.activity.borrow().connections,
        "browser disconnected"
    );
}

async fn shutdown_signal(
    mut activity_updates: watch::Receiver<BrowserActivity>,
    timeout: Duration,
) {
    loop {
        if activity_updates.borrow_and_update().connections == 0 {
            tracing::debug!(
                timeout_seconds = timeout.as_secs(),
                "waiting for browser activity"
            );
            tokio::select! {
                () = tokio::time::sleep(timeout) => {
                    tracing::info!("browser activity timeout elapsed; shutting down");
                    return;
                }
                changed = activity_updates.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "failed to listen for Ctrl-C");
                    }
                    tracing::info!("received Ctrl-C; shutting down");
                    return;
                }
            }
        } else {
            tokio::select! {
                changed = activity_updates.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "failed to listen for Ctrl-C");
                    }
                    tracing::info!("received Ctrl-C; shutting down");
                    return;
                }
            }
        }
    }
}

fn load_versions(input_paths: &[PathBuf]) -> Result<Vec<SnapshotVersions>> {
    let current_paths = input_paths
        .iter()
        .map(|input_path| canonicalize_svg(input_path))
        .collect::<Result<Vec<_>>>()?;
    let first_path = current_paths
        .first()
        .context("at least one SVG path is required")?;
    let first_parent = first_path
        .parent()
        .context("the SVG path does not have a parent directory")?;
    let repository = gix::discover(first_parent).with_context(|| {
        format!(
            "failed to discover a Git repository containing '{}'",
            first_path.display()
        )
    })?;
    let worktree = canonicalize_worktree(&repository, first_path)?;

    for current_path in &current_paths {
        let parent = current_path
            .parent()
            .context("the SVG path does not have a parent directory")?;
        let current_repository = gix::discover(parent).with_context(|| {
            format!(
                "failed to discover a Git repository containing '{}'",
                current_path.display()
            )
        })?;
        let current_worktree = canonicalize_worktree(&current_repository, current_path)?;
        if current_worktree != worktree {
            bail!(
                "SVG '{}' belongs to Git worktree '{}', but expected worktree '{}'",
                current_path.display(),
                current_worktree.display(),
                worktree.display()
            );
        }
    }

    let head = repository
        .head_commit()
        .context("failed to resolve HEAD to a commit; the repository may not have any commits")?;
    let tree = head.tree().context("failed to load the tree at HEAD")?;

    current_paths
        .into_iter()
        .map(|current_path| {
            let repository_relative_path = current_path
                .strip_prefix(&worktree)
                .with_context(|| {
                    format!(
                        "SVG '{}' is outside repository working tree '{}'",
                        current_path.display(),
                        worktree.display()
                    )
                })?
                .to_owned();
            let current = fs::read(&current_path).with_context(|| {
                format!("failed to read current SVG at '{}'", current_path.display())
            })?;
            let entry = tree
                .lookup_entry_by_path(&repository_relative_path)
                .with_context(|| {
                    format!(
                        "failed to look up '{}' in the tree at HEAD",
                        repository_relative_path.display()
                    )
                })?;
            let previous = match entry {
                Some(entry) => {
                    let object = entry.object().with_context(|| {
                        format!(
                            "failed to load '{}' from HEAD",
                            repository_relative_path.display()
                        )
                    })?;
                    let blob = object.try_into_blob().with_context(|| {
                        format!(
                            "'{}' is not a file at HEAD",
                            repository_relative_path.display()
                        )
                    })?;
                    Some(blob.data.clone())
                }
                None => None,
            };

            Ok(SnapshotVersions {
                repository_relative_path,
                previous,
                current,
            })
        })
        .collect()
}

fn canonicalize_svg(input_path: &Path) -> Result<PathBuf> {
    if !input_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    {
        bail!("expected an .svg file, got '{}'", input_path.display());
    }

    let current_path = fs::canonicalize(input_path)
        .with_context(|| format!("failed to find SVG at '{}'", input_path.display()))?;

    if !current_path.is_file() {
        bail!("'{}' is not a file", current_path.display());
    }

    Ok(current_path)
}

fn canonicalize_worktree(repository: &gix::Repository, svg_path: &Path) -> Result<PathBuf> {
    let worktree = repository
        .workdir()
        .context("the discovered Git repository does not have a working tree")?;
    fs::canonicalize(worktree).with_context(|| {
        format!(
            "failed to resolve Git worktree '{}' containing '{}'",
            worktree.display(),
            svg_path.display()
        )
    })
}

fn render_html(versions: &[SnapshotVersions]) -> String {
    let Some(first) = versions.first() else {
        return String::new();
    };
    let first_path = escape_html(&first.repository_relative_path.to_string_lossy());
    let first_has_previous = first.previous.is_some();
    let mode_switcher_hidden = if first_has_previous { "" } else { " hidden" };
    let file_count = versions.len();
    let file_count_label = if file_count == 1 { "file" } else { "files" };
    let mut file_buttons = String::new();
    let mut file_data = String::new();

    for (index, versions) in versions.iter().enumerate() {
        let path = escape_html(&versions.repository_relative_path.to_string_lossy());
        let file_name = versions
            .repository_relative_path
            .file_name()
            .unwrap_or(versions.repository_relative_path.as_os_str());
        let file_name = escape_html(&file_name.to_string_lossy());
        let is_selected = index == 0;
        let (has_previous, previous) = match &versions.previous {
            Some(previous) => (true, STANDARD.encode(previous)),
            None => (false, String::new()),
        };
        write!(
            file_buttons,
            r#"<button class="file-button" type="button" data-file-index="{index}" aria-current="{is_selected}">{file_name}</button>"#
        )
        .expect("writing HTML to a string cannot fail");
        write!(
            file_data,
            r#"<div data-file data-path="{path}" data-has-previous="{has_previous}" data-previous="{previous}" data-current="{}"></div>"#,
            STANDARD.encode(&versions.current)
        )
        .expect("writing HTML to a string cannot fail");
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark">
  <title>SVG comparison — {first_path}</title>
  <style>
    :root {{
      color-scheme: dark;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #111318;
      color: #eceff4;
    }}
    * {{ box-sizing: border-box; }}
    [hidden] {{ display: none !important; }}
    body {{ margin: 0; min-height: 100vh; }}
    .app-shell {{
      display: grid;
      grid-template-columns: minmax(14rem, 20rem) minmax(0, 1fr);
      min-height: 100vh;
    }}
    .app-shell[data-sidebar-hidden="true"] {{
      grid-template-columns: minmax(0, 1fr);
    }}
    .app-shell[data-sidebar-hidden="true"] > .file-sidebar {{
      display: none;
    }}
    .file-sidebar {{
      position: sticky;
      top: 0;
      height: 100vh;
      overflow: auto;
      border-right: 1px solid #343944;
      background: #181b21;
    }}
    .file-list-heading {{
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 1rem;
      padding: 1rem;
      border-bottom: 1px solid #343944;
      font-size: 0.95rem;
      font-weight: 600;
    }}
    .file-count {{
      color: #8c96a8;
      font-size: 0.75rem;
      font-weight: 400;
    }}
    .file-list {{
      display: grid;
    }}
    .file-button {{
      width: 100%;
      border: 0;
      border-bottom: 1px solid #2b303a;
      border-left: 3px solid transparent;
      padding: 0.75rem 1rem;
      background: transparent;
      color: #aeb6c5;
      cursor: pointer;
      font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      font-size: 0.8rem;
      line-height: 1.4;
      overflow-wrap: anywhere;
      text-align: left;
    }}
    .file-button:hover {{
      background: #242832;
      color: #eceff4;
    }}
    .file-button[aria-current="true"] {{
      border-left-color: #2f81f7;
      background: #1f6feb26;
      color: #fff;
    }}
    .content {{ min-width: 0; }}
    header {{
      padding: 1rem 1.25rem;
      border-bottom: 1px solid #343944;
      background: #181b21;
    }}
    h1 {{ margin: 0 0 0.35rem; font-size: 1.1rem; }}
    .path {{
      color: #aeb6c5;
      font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      font-size: 0.85rem;
      overflow-wrap: anywhere;
    }}
    .toolbar {{
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: 1rem;
      margin-top: 1rem;
    }}
    .sidebar-toggle {{
      border: 1px solid #454b59;
      border-radius: 0.4rem;
      padding: 0.55rem 0.85rem;
      background: #242832;
      color: #d8dee9;
      cursor: pointer;
      font: inherit;
      font-size: 0.85rem;
    }}
    .sidebar-toggle:hover {{ background: #303642; }}
    .mode-switcher {{
      display: inline-flex;
      overflow: hidden;
      border: 1px solid #454b59;
      border-radius: 0.4rem;
    }}
    .mode-button {{
      border: 0;
      border-right: 1px solid #454b59;
      padding: 0.55rem 0.85rem;
      background: #242832;
      color: #d8dee9;
      cursor: pointer;
      font: inherit;
      font-size: 0.85rem;
    }}
    .mode-button:last-child {{ border-right: 0; }}
    .mode-button:hover {{ background: #303642; }}
    .mode-button[aria-pressed="true"] {{
      background: #0969da;
      color: #fff;
    }}
    .file-button:focus-visible,
    .sidebar-toggle:focus-visible,
    .mode-button:focus-visible,
    input[type="range"]:focus-visible {{
      outline: 2px solid #58a6ff;
      outline-offset: -2px;
    }}
    .mode-control {{
      display: flex;
      align-items: center;
      gap: 0.65rem;
      color: #c5ccd8;
      font-size: 0.85rem;
    }}
    .mode-control input {{
      width: min(16rem, 40vw);
      accent-color: #2f81f7;
    }}
    .mode-control output {{
      min-width: 3.25rem;
      color: #fff;
      font-variant-numeric: tabular-nums;
    }}
    .comparison {{
      --onion-opacity: 0.5;
      --swipe-position: 50%;
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 1rem;
      padding: 1rem;
    }}
    .comparison section {{
      min-width: 0;
      overflow: hidden;
      border: 1px solid #343944;
      border-radius: 0.5rem;
      background: #181b21;
    }}
    .comparison h2 {{
      margin: 0;
      padding: 0.75rem 1rem;
      border-bottom: 1px solid #343944;
      font-size: 0.95rem;
    }}
    .viewport {{
      overflow: auto;
      padding: 1rem;
      background-color: #242832;
      background-image:
        linear-gradient(45deg, #2b303b 25%, transparent 25%),
        linear-gradient(-45deg, #2b303b 25%, transparent 25%),
        linear-gradient(45deg, transparent 75%, #2b303b 75%),
        linear-gradient(-45deg, transparent 75%, #2b303b 75%);
      background-position: 0 0, 0 8px, 8px -8px, -8px 0;
      background-size: 16px 16px;
    }}
    img {{
      display: block;
      width: 100%;
      height: auto;
      user-select: none;
      -webkit-user-drag: none;
    }}
    .overlay-panel {{ display: none; }}
    .comparison[data-mode="onion"],
    .comparison[data-mode="swipe"] {{
      grid-template-columns: minmax(0, 1fr);
    }}
    .comparison[data-mode="onion"] .side-by-side-panel,
    .comparison[data-mode="swipe"] .side-by-side-panel {{
      display: none;
    }}
    .comparison[data-mode="onion"] .overlay-panel,
    .comparison[data-mode="swipe"] .overlay-panel {{
      display: block;
    }}
    .comparison[data-has-previous="false"] {{
      grid-template-columns: minmax(0, 1fr);
    }}
    .comparison[data-has-previous="false"] .previous-panel,
    .comparison[data-has-previous="false"] .overlay-panel {{
      display: none;
    }}
    .comparison[data-has-previous="false"] .current-panel {{
      display: block;
    }}
    .overlay-viewport {{ padding: 1rem; }}
    .image-stack {{
      position: relative;
      display: grid;
      overflow: hidden;
      isolation: isolate;
    }}
    .image-stack > img {{
      grid-area: 1 / 1;
      align-self: start;
      pointer-events: none;
    }}
    .previous-overlay {{ z-index: 1; }}
    .current-overlay {{
      z-index: 2;
      opacity: var(--onion-opacity);
    }}
    .comparison[data-mode="swipe"] .image-stack {{
      cursor: ew-resize;
      touch-action: none;
    }}
    .comparison[data-mode="swipe"] .current-overlay {{
      opacity: 1;
      clip-path: inset(0 0 0 var(--swipe-position));
    }}
    .overlay-label {{
      position: absolute;
      z-index: 3;
      top: 0.75rem;
      padding: 0.3rem 0.5rem;
      border: 1px solid #596273;
      border-radius: 0.3rem;
      background: rgb(17 19 24 / 85%);
      color: #fff;
      font-size: 0.75rem;
      font-weight: 600;
      pointer-events: none;
    }}
    .label-previous {{ left: 0.75rem; }}
    .label-current {{ right: 0.75rem; }}
    .swipe-handle {{
      position: absolute;
      z-index: 4;
      top: 0;
      bottom: 0;
      left: var(--swipe-position);
      width: 2.5rem;
      transform: translateX(-50%);
      cursor: ew-resize;
    }}
    .swipe-handle::before {{
      position: absolute;
      top: 0;
      bottom: 0;
      left: calc(50% - 1px);
      width: 2px;
      background: #fff;
      box-shadow: 0 0 0 1px rgb(0 0 0 / 50%);
      content: "";
    }}
    .swipe-handle::after {{
      position: absolute;
      top: 50%;
      left: 50%;
      display: grid;
      width: 2rem;
      height: 2rem;
      transform: translate(-50%, -50%);
      place-items: center;
      border: 2px solid #fff;
      border-radius: 50%;
      background: #0969da;
      box-shadow: 0 1px 4px rgb(0 0 0 / 60%);
      color: #fff;
      content: "↔";
      font-size: 1rem;
      line-height: 1;
    }}
    .comparison[data-mode="onion"] .swipe-handle {{ display: none; }}
    @media (max-width: 48rem) {{
      .app-shell {{ grid-template-columns: minmax(0, 1fr); }}
      .file-sidebar {{
        position: static;
        height: auto;
        max-height: 40vh;
        border-right: 0;
        border-bottom: 1px solid #343944;
      }}
    }}
  </style>
</head>
<body>
  <div class="app-shell" id="app-shell">
    <aside class="file-sidebar" id="file-sidebar">
      <div class="file-list-heading">
        <span>Files</span>
        <span class="file-count">{file_count} {file_count_label}</span>
      </div>
      <nav class="file-list" aria-label="SVG files">{file_buttons}</nav>
      <div id="file-data" hidden>{file_data}</div>
    </aside>
    <div class="content">
      <header>
        <h1>SVG comparison</h1>
        <div class="path" id="selected-path">{first_path}</div>
        <div class="toolbar">
          <button class="sidebar-toggle" id="sidebar-toggle" type="button" aria-controls="file-sidebar" aria-expanded="true">Hide files</button>
          <div class="mode-switcher" id="mode-switcher" role="group" aria-label="Comparison mode"{mode_switcher_hidden}>
            <button class="mode-button" type="button" data-mode-button="side-by-side" aria-pressed="true">Side by side</button>
            <button class="mode-button" type="button" data-mode-button="onion" aria-pressed="false">Onion skin</button>
            <button class="mode-button" type="button" data-mode-button="swipe" aria-pressed="false">Swipe</button>
          </div>
          <div class="mode-control" id="onion-control" hidden>
            <label for="onion-opacity">Current opacity</label>
            <input id="onion-opacity" type="range" min="0" max="100" value="50">
            <output id="onion-output" for="onion-opacity">50%</output>
          </div>
          <div class="mode-control" id="swipe-control" hidden>
            <label for="swipe-position">Divider position</label>
            <input id="swipe-position" type="range" min="0" max="100" value="50">
            <output id="swipe-output" for="swipe-position">50%</output>
          </div>
        </div>
      </header>
      <main class="comparison" id="comparison" data-mode="side-by-side" data-has-previous="{first_has_previous}">
        <section class="side-by-side-panel previous-panel">
          <h2>Previous (HEAD)</h2>
          <div class="viewport">
            <img id="previous-image" alt="Previous SVG at HEAD" draggable="false">
          </div>
        </section>
        <section class="side-by-side-panel current-panel">
          <h2>Current (working tree)</h2>
          <div class="viewport">
            <img id="current-image" alt="Current SVG in the working tree" draggable="false">
          </div>
        </section>
        <section class="overlay-panel">
          <h2 id="overlay-title">Overlay comparison</h2>
          <div class="viewport overlay-viewport">
            <div class="image-stack" id="image-stack">
              <img class="previous-overlay" id="previous-overlay-image" alt="Previous SVG at HEAD" draggable="false">
              <img class="current-overlay" id="current-overlay-image" alt="Current SVG in the working tree" draggable="false">
              <span class="overlay-label label-previous">Previous</span>
              <span class="overlay-label label-current">Current</span>
              <div class="swipe-handle" aria-hidden="true"></div>
            </div>
          </div>
        </section>
      </main>
    </div>
  </div>
  <script src="/lifecycle.js"></script>
  <script>
    (() => {{
      "use strict";

      const appShell = document.getElementById("app-shell");
      const sidebarToggle = document.getElementById("sidebar-toggle");
      const comparison = document.getElementById("comparison");
      const modeSwitcher = document.getElementById("mode-switcher");
      const modeButtons = Array.from(document.querySelectorAll("[data-mode-button]"));
      const onionControl = document.getElementById("onion-control");
      const onionRange = document.getElementById("onion-opacity");
      const onionOutput = document.getElementById("onion-output");
      const swipeControl = document.getElementById("swipe-control");
      const swipeRange = document.getElementById("swipe-position");
      const swipeOutput = document.getElementById("swipe-output");
      const overlayTitle = document.getElementById("overlay-title");
      const imageStack = document.getElementById("image-stack");
      const selectedPath = document.getElementById("selected-path");
      const fileButtons = Array.from(document.querySelectorAll("[data-file-index]"));
      const files = Array.from(document.querySelectorAll("[data-file]"));
      const viewports = Array.from(document.querySelectorAll(".viewport"));
      const previousImages = [
        document.getElementById("previous-image"),
        document.getElementById("previous-overlay-image"),
      ];
      const currentImages = [
        document.getElementById("current-image"),
        document.getElementById("current-overlay-image"),
      ];
      let currentFileIndex = 0;

      const clampPercentage = (value) => Math.min(100, Math.max(0, Number(value)));

      const setSidebarVisible = (isVisible) => {{
        appShell.dataset.sidebarHidden = String(!isVisible);
        sidebarToggle.setAttribute("aria-expanded", String(isVisible));
        sidebarToggle.textContent = isVisible ? "Hide files" : "Show files";
      }};

      const updateModeControls = () => {{
        const hasPrevious = comparison.dataset.hasPrevious === "true";
        const mode = comparison.dataset.mode;
        modeSwitcher.hidden = !hasPrevious;
        onionControl.hidden = !hasPrevious || mode !== "onion";
        swipeControl.hidden = !hasPrevious || mode !== "swipe";
      }};

      const setFile = (index) => {{
        currentFileIndex = index;
        const file = files[index];
        const path = file.dataset.path;
        const hasPrevious = file.dataset.hasPrevious === "true";
        const currentSource = "data:image/svg+xml;base64," + file.dataset.current;

        fileButtons.forEach((button, buttonIndex) => {{
          button.setAttribute("aria-current", String(buttonIndex === index));
        }});
        previousImages.forEach((image) => {{
          if (hasPrevious) {{
            image.src = "data:image/svg+xml;base64," + file.dataset.previous;
            image.alt = "Previous version of " + path + " at HEAD";
          }} else {{
            image.removeAttribute("src");
            image.alt = "";
          }}
        }});
        currentImages.forEach((image) => {{
          image.src = currentSource;
          image.alt = "Current working-tree version of " + path;
        }});
        comparison.dataset.hasPrevious = String(hasPrevious);
        updateModeControls();
        selectedPath.textContent = path;
        document.title = "SVG comparison — " + path;
        viewports.forEach((viewport) => viewport.scrollTo(0, 0));
      }};

      const updateOnionOpacity = (value) => {{
        const percentage = clampPercentage(value);
        comparison.style.setProperty("--onion-opacity", String(percentage / 100));
        onionOutput.textContent = Math.round(percentage) + "%";
      }};

      const updateSwipePosition = (value) => {{
        const percentage = clampPercentage(value);
        comparison.style.setProperty("--swipe-position", percentage + "%");
        swipeOutput.textContent = Math.round(percentage) + "%";
      }};

      const setMode = (mode) => {{
        comparison.dataset.mode = mode;
        modeButtons.forEach((button) => {{
          button.setAttribute("aria-pressed", String(button.dataset.modeButton === mode));
        }});
        updateModeControls();
        overlayTitle.textContent = mode === "onion" ? "Onion skin" : "Swipe";
      }};

      modeButtons.forEach((button) => {{
        button.addEventListener("click", () => setMode(button.dataset.modeButton));
      }});
      sidebarToggle.addEventListener("click", () => {{
        const isVisible = appShell.dataset.sidebarHidden !== "true";
        setSidebarVisible(!isVisible);
      }});
      fileButtons.forEach((button, index) => {{
        button.addEventListener("click", () => setFile(index));
        button.addEventListener("keydown", (event) => {{
          let nextIndex = null;
          if (event.key === "Home") {{
            nextIndex = 0;
          }} else if (event.key === "End") {{
            nextIndex = fileButtons.length - 1;
          }}

          if (nextIndex !== null) {{
            event.preventDefault();
            fileButtons[nextIndex].focus();
            setFile(nextIndex);
          }}
        }});
      }});
      document.addEventListener("keydown", (event) => {{
        if (
          event.key !== "ArrowDown" &&
          event.key !== "ArrowUp" &&
          event.key !== "j" &&
          event.key !== "k"
        ) {{
          return;
        }}

        event.preventDefault();
        const direction = event.key === "ArrowDown" || event.key === "j" ? 1 : -1;
        const nextIndex = Math.min(
          fileButtons.length - 1,
          Math.max(0, currentFileIndex + direction),
        );
        const shouldMoveFocus = fileButtons.includes(document.activeElement);
        setFile(nextIndex);
        fileButtons[nextIndex].scrollIntoView({{ block: "nearest" }});
        if (shouldMoveFocus) {{
          fileButtons[nextIndex].focus();
        }}
      }});
      onionRange.addEventListener("input", () => updateOnionOpacity(onionRange.value));
      swipeRange.addEventListener("input", () => updateSwipePosition(swipeRange.value));

      const updateSwipeFromPointer = (event) => {{
        const bounds = imageStack.getBoundingClientRect();
        const percentage = ((event.clientX - bounds.left) / bounds.width) * 100;
        const roundedPercentage = Math.round(clampPercentage(percentage));
        swipeRange.value = String(roundedPercentage);
        updateSwipePosition(roundedPercentage);
      }};

      let activePointer = null;
      imageStack.addEventListener("pointerdown", (event) => {{
        if (comparison.dataset.mode !== "swipe" || event.button !== 0) {{
          return;
        }}
        activePointer = event.pointerId;
        imageStack.setPointerCapture(event.pointerId);
        updateSwipeFromPointer(event);
      }});
      imageStack.addEventListener("pointermove", (event) => {{
        if (event.pointerId === activePointer) {{
          updateSwipeFromPointer(event);
        }}
      }});
      imageStack.addEventListener("pointerup", (event) => {{
        if (event.pointerId === activePointer) {{
          activePointer = null;
          imageStack.releasePointerCapture(event.pointerId);
        }}
      }});
      imageStack.addEventListener("pointercancel", () => {{
        activePointer = null;
      }});

      updateOnionOpacity(onionRange.value);
      updateSwipePosition(swipeRange.value);
      setSidebarVisible(true);
      setFile(0);
      setMode("side-by-side");
    }})();
  </script>
</body>
</html>
"#
    )
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_paths() {
        let cli = Cli::try_parse_from(["compare-svg", "snapshot.svg", "icons/other.svg"])
            .expect("arguments should parse");

        assert!(cli.command.is_none());
        assert!(cli.directory.is_none());
        assert_eq!(
            cli.paths,
            [
                PathBuf::from("snapshot.svg"),
                PathBuf::from("icons/other.svg")
            ]
        );
    }

    #[test]
    fn parses_working_directory() {
        let cli = Cli::try_parse_from(["compare-svg", "-C", "/tmp/repository", "snapshot.svg"])
            .expect("working directory should parse");

        assert_eq!(cli.directory, Some(PathBuf::from("/tmp/repository")));
        assert_eq!(cli.paths, [PathBuf::from("snapshot.svg")]);
    }

    #[test]
    fn parses_working_directory_after_serve_subcommand() {
        let cli = Cli::try_parse_from([
            "compare-svg",
            "serve",
            "-C",
            "/tmp/repository",
            "snapshot.svg",
        ])
        .expect("global working directory should parse after the subcommand");

        assert_eq!(cli.directory, Some(PathBuf::from("/tmp/repository")));
        let Some(Commands::Serve(args)) = cli.command else {
            panic!("expected the serve command");
        };
        assert_eq!(args.paths, [PathBuf::from("snapshot.svg")]);
    }

    #[test]
    fn parses_serve_timeout_and_paths() {
        let cli = Cli::try_parse_from(["compare-svg", "serve", "--timeout", "120", "snapshot.svg"])
            .expect("serve arguments should parse");

        let Some(Commands::Serve(args)) = cli.command else {
            panic!("expected the serve command");
        };
        assert_eq!(args.timeout, 120);
        assert_eq!(args.paths, [PathBuf::from("snapshot.svg")]);
    }

    #[test]
    fn requires_at_least_one_path() {
        assert!(Cli::try_parse_from(["compare-svg"]).is_err());
        assert!(Cli::try_parse_from(["compare-svg", "serve"]).is_err());
    }

    #[test]
    fn renders_self_contained_comparisons() {
        let versions = [
            SnapshotVersions {
                repository_relative_path: PathBuf::from("snapshots/<example>&.svg"),
                previous: Some(b"previous".to_vec()),
                current: b"current".to_vec(),
            },
            SnapshotVersions {
                repository_relative_path: PathBuf::from("icons/other.svg"),
                previous: None,
                current: b"other current".to_vec(),
            },
        ];

        let html = render_html(&versions);

        assert!(html.contains("snapshots/&lt;example&gt;&amp;.svg"));
        assert!(html.contains("icons/other.svg"));
        assert!(html.contains("2 files"));
        assert!(html.contains("Previous (HEAD)"));
        assert!(html.contains("Current (working tree)"));
        assert_eq!(html.matches("class=\"file-button\"").count(), 2);
        assert!(html.contains("data-file-index=\"0\" aria-current=\"true\""));
        assert!(html.contains("data-file-index=\"1\" aria-current=\"false\""));
        assert!(html.contains("aria-current=\"true\">&lt;example&gt;&amp;.svg</button>"));
        assert!(html.contains("aria-current=\"false\">other.svg</button>"));
        assert!(!html.contains(">snapshots/&lt;example&gt;&amp;.svg</button>"));
        assert!(!html.contains(">icons/other.svg</button>"));
        assert!(html.contains("data-mode=\"side-by-side\""));
        assert!(html.contains("data-mode-button=\"onion\""));
        assert!(html.contains("data-mode-button=\"swipe\""));
        assert!(html.contains("id=\"onion-opacity\""));
        assert!(html.contains("id=\"swipe-position\""));
        assert!(html.contains("data-has-previous=\"true\" data-previous="));
        assert!(html.contains(
            "data-path=\"icons/other.svg\" data-has-previous=\"false\" data-previous=\"\""
        ));
        assert!(html.contains(".comparison[data-has-previous=\"false\"]"));
        assert!(html.contains("modeSwitcher.hidden = !hasPrevious"));
        assert!(html.contains(
            "id=\"sidebar-toggle\" type=\"button\" aria-controls=\"file-sidebar\" aria-expanded=\"true\">Hide files</button>"
        ));
        assert!(html.contains(".app-shell[data-sidebar-hidden=\"true\"]"));
        assert!(html.contains("setSidebarVisible(true)"));
        assert!(html.contains("setFile(0)"));
        assert!(html.contains("<script src=\"/lifecycle.js\"></script>"));
        assert!(!html.contains("connectLifecycleSocket"));
        assert!(html.contains("document.addEventListener(\"keydown\""));
        assert!(html.contains("event.key !== \"ArrowDown\""));
        assert!(html.contains("event.key !== \"ArrowUp\""));
        assert!(html.contains("event.key !== \"j\""));
        assert!(html.contains("event.key !== \"k\""));
        assert!(html.contains("event.key === \"ArrowDown\" || event.key === \"j\" ? 1 : -1"));
        assert!(html.contains("fileButtons[nextIndex].scrollIntoView"));
        assert_eq!(
            html.matches(&format!(
                "data-previous=\"{}\"",
                STANDARD.encode(b"previous")
            ))
            .count(),
            1
        );
        assert_eq!(
            html.matches(&format!("data-current=\"{}\"", STANDARD.encode(b"current")))
                .count(),
            1
        );
        assert_eq!(
            html.matches(&format!(
                "data-current=\"{}\"",
                STANDARD.encode(b"other current")
            ))
            .count(),
            1
        );
        assert_eq!(html.matches("draggable=\"false\"").count(), 4);
        assert!(html.contains("pointer-events: none"));
        assert!(!html.contains("<example>"));
    }

    #[test]
    fn renders_an_untracked_initial_file_as_current_only() {
        let versions = [SnapshotVersions {
            repository_relative_path: PathBuf::from("icons/new.svg"),
            previous: None,
            current: b"new current".to_vec(),
        }];

        let html = render_html(&versions);

        assert!(
            html.contains(
                "id=\"mode-switcher\" role=\"group\" aria-label=\"Comparison mode\" hidden"
            )
        );
        assert!(
            html.contains(
                "id=\"comparison\" data-mode=\"side-by-side\" data-has-previous=\"false\""
            )
        );
        assert!(html.contains(
            "data-path=\"icons/new.svg\" data-has-previous=\"false\" data-previous=\"\""
        ));
        assert_eq!(
            html.matches(&format!(
                "data-current=\"{}\"",
                STANDARD.encode(b"new current")
            ))
            .count(),
            1
        );
    }

    #[test]
    fn escapes_html_special_characters() {
        assert_eq!(escape_html("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }
}
