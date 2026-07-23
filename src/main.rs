use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::Parser;
use tempdir::TempDir;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Compare an SVG in the working tree with its version at Git HEAD"
)]
struct Cli {
    /// Open the generated comparison in the default browser
    #[arg(long)]
    open: bool,

    /// Path to an SVG in a Git working tree
    path: PathBuf,
}

#[derive(Debug)]
struct SnapshotVersions {
    repository_relative_path: PathBuf,
    previous: Vec<u8>,
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
    let versions = load_versions(&cli.path)?;
    let html = render_html(&versions);
    let output_path = write_to_temp_dir(&html)?;

    println!("{}", output_path.display());

    if cli.open {
        open::that(&output_path).with_context(|| {
            format!(
                "failed to open '{}' in the default browser",
                output_path.display()
            )
        })?;
    }

    Ok(())
}

fn load_versions(input_path: &Path) -> Result<SnapshotVersions> {
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

    let current = fs::read(&current_path)
        .with_context(|| format!("failed to read current SVG at '{}'", current_path.display()))?;

    let parent = current_path
        .parent()
        .context("the SVG path does not have a parent directory")?;
    let repository = gix::discover(parent).with_context(|| {
        format!(
            "failed to discover a Git repository containing '{}'",
            current_path.display()
        )
    })?;
    let worktree = repository
        .workdir()
        .context("the discovered Git repository does not have a working tree")?;
    let worktree = fs::canonicalize(worktree).with_context(|| {
        format!(
            "failed to resolve repository working tree at '{}'",
            worktree.display()
        )
    })?;
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

    let head = repository
        .head_commit()
        .context("failed to resolve HEAD to a commit; the repository may not have any commits")?;
    let tree = head.tree().context("failed to load the tree at HEAD")?;
    let entry = tree
        .lookup_entry_by_path(&repository_relative_path)
        .with_context(|| {
            format!(
                "failed to look up '{}' in the tree at HEAD",
                repository_relative_path.display()
            )
        })?
        .with_context(|| {
            format!(
                "'{}' does not have a previous version at HEAD",
                repository_relative_path.display()
            )
        })?;
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

    Ok(SnapshotVersions {
        repository_relative_path,
        previous: blob.data.clone(),
        current,
    })
}

fn render_html(versions: &SnapshotVersions) -> String {
    let path = escape_html(&versions.repository_relative_path.to_string_lossy());
    let previous = STANDARD.encode(&versions.previous);
    let current = STANDARD.encode(&versions.current);

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="dark">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'">
  <title>SVG comparison — {path}</title>
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
    .mode-button:focus-visible,
    input[type="range"]:focus-visible {{
      outline: 2px solid #58a6ff;
      outline-offset: 2px;
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
  </style>
</head>
<body>
  <header>
    <h1>SVG comparison</h1>
    <div class="path">{path}</div>
    <div class="toolbar">
      <div class="mode-switcher" role="group" aria-label="Comparison mode">
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
  <main class="comparison" id="comparison" data-mode="side-by-side">
    <section class="side-by-side-panel">
      <h2>Previous (HEAD)</h2>
      <div class="viewport">
        <img src="data:image/svg+xml;base64,{previous}" alt="Previous SVG at HEAD" draggable="false">
      </div>
    </section>
    <section class="side-by-side-panel">
      <h2>Current (working tree)</h2>
      <div class="viewport">
        <img src="data:image/svg+xml;base64,{current}" alt="Current SVG in the working tree" draggable="false">
      </div>
    </section>
    <section class="overlay-panel">
      <h2 id="overlay-title">Overlay comparison</h2>
      <div class="viewport overlay-viewport">
        <div class="image-stack" id="image-stack">
          <img class="previous-overlay" src="data:image/svg+xml;base64,{previous}" alt="Previous SVG at HEAD" draggable="false">
          <img class="current-overlay" src="data:image/svg+xml;base64,{current}" alt="Current SVG in the working tree" draggable="false">
          <span class="overlay-label label-previous">Previous</span>
          <span class="overlay-label label-current">Current</span>
          <div class="swipe-handle" aria-hidden="true"></div>
        </div>
      </div>
    </section>
  </main>
  <script>
    (() => {{
      "use strict";

      const comparison = document.getElementById("comparison");
      const modeButtons = Array.from(document.querySelectorAll("[data-mode-button]"));
      const onionControl = document.getElementById("onion-control");
      const onionRange = document.getElementById("onion-opacity");
      const onionOutput = document.getElementById("onion-output");
      const swipeControl = document.getElementById("swipe-control");
      const swipeRange = document.getElementById("swipe-position");
      const swipeOutput = document.getElementById("swipe-output");
      const overlayTitle = document.getElementById("overlay-title");
      const imageStack = document.getElementById("image-stack");

      const clampPercentage = (value) => Math.min(100, Math.max(0, Number(value)));

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
        onionControl.hidden = mode !== "onion";
        swipeControl.hidden = mode !== "swipe";
        overlayTitle.textContent = mode === "onion" ? "Onion skin" : "Swipe";
      }};

      modeButtons.forEach((button) => {{
        button.addEventListener("click", () => setMode(button.dataset.modeButton));
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
      setMode("side-by-side");
    }})();
  </script>
</body>
</html>
"#
    )
}

fn write_to_temp_dir(html: &str) -> Result<PathBuf> {
    let temp_dir = TempDir::new("compare-svg")
        .context("failed to create a temporary directory for the comparison")?;
    let output_path = temp_dir.path().join("comparison.html");

    fs::write(&output_path, html).with_context(|| {
        format!(
            "failed to write comparison HTML to '{}'",
            output_path.display()
        )
    })?;

    let persisted_dir = temp_dir.into_path();
    Ok(persisted_dir.join("comparison.html"))
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
    fn parses_open_before_path() {
        let cli = Cli::try_parse_from(["compare-svg", "--open", "snapshot.svg"])
            .expect("arguments should parse");

        assert!(cli.open);
        assert_eq!(cli.path, PathBuf::from("snapshot.svg"));
    }

    #[test]
    fn renders_self_contained_comparison() {
        let versions = SnapshotVersions {
            repository_relative_path: PathBuf::from("snapshots/<example>&.svg"),
            previous: b"previous".to_vec(),
            current: b"current".to_vec(),
        };

        let html = render_html(&versions);

        assert!(html.contains("snapshots/&lt;example&gt;&amp;.svg"));
        assert!(html.contains("Previous (HEAD)"));
        assert!(html.contains("Current (working tree)"));
        assert!(html.contains("data-mode=\"side-by-side\""));
        assert!(html.contains("data-mode-button=\"onion\""));
        assert!(html.contains("data-mode-button=\"swipe\""));
        assert!(html.contains("id=\"onion-opacity\""));
        assert!(html.contains("id=\"swipe-position\""));
        assert_eq!(html.matches(&STANDARD.encode(b"previous")).count(), 2);
        assert_eq!(html.matches(&STANDARD.encode(b"current")).count(), 2);
        assert_eq!(html.matches("draggable=\"false\"").count(), 4);
        assert!(html.contains("pointer-events: none"));
        assert!(!html.contains("<example>"));
    }

    #[test]
    fn persists_generated_html() {
        let output_path = write_to_temp_dir("comparison").expect("HTML should be written");
        let output_dir = output_path
            .parent()
            .expect("output should have a parent")
            .to_owned();

        assert_eq!(
            fs::read_to_string(&output_path).expect("HTML should remain readable"),
            "comparison"
        );

        fs::remove_dir_all(output_dir).expect("test output should be removable");
    }

    #[test]
    fn escapes_html_special_characters() {
        assert_eq!(escape_html("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }
}
