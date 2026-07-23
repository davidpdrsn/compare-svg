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
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'">
  <title>SVG comparison — {path}</title>
  <style>
    :root {{
      color-scheme: dark;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #111318;
      color: #eceff4;
    }}
    * {{ box-sizing: border-box; }}
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
    main {{
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 1rem;
      padding: 1rem;
    }}
    section {{
      min-width: 0;
      overflow: hidden;
      border: 1px solid #343944;
      border-radius: 0.5rem;
      background: #181b21;
    }}
    h2 {{
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
    img {{ display: block; width: 100%; height: auto; }}
  </style>
</head>
<body>
  <header>
    <h1>SVG comparison</h1>
    <div class="path">{path}</div>
  </header>
  <main>
    <section>
      <h2>Previous (HEAD)</h2>
      <div class="viewport">
        <img src="data:image/svg+xml;base64,{previous}" alt="Previous SVG at HEAD">
      </div>
    </section>
    <section>
      <h2>Current (working tree)</h2>
      <div class="viewport">
        <img src="data:image/svg+xml;base64,{current}" alt="Current SVG in the working tree">
      </div>
    </section>
  </main>
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
        assert!(html.contains(&STANDARD.encode(b"previous")));
        assert!(html.contains(&STANDARD.encode(b"current")));
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
