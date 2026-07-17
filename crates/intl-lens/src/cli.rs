use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use react_i18next_lens::analysis::{AnalyzerConfig, ReactSourceAnalyzer};
use react_i18next_lens::domain::KeyResolution;
use react_i18next_lens::domain::TranslationKey;
use react_i18next_lens::mutation::AddMissingKey;
use react_i18next_lens::workspace::Workspace;

#[derive(Parser)]
#[command(name = "react-i18next-lens-cli")]
#[command(about = "Audit and analyze React i18next workspaces")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to the project root
    #[arg(short, long, default_value = ".", global = true)]
    workspace: PathBuf,

    /// Output format
    #[arg(short, long, value_enum, default_value = "terminal", global = true)]
    format: OutputFormat,

    /// Output file (if not specified, prints to stdout)
    #[arg(short, long, global = true)]
    output: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Audit the entire project for i18n issues
    Audit {
        /// Filter by missing locales (comma-separated)
        #[arg(long)]
        missing_in: Option<String>,

        /// Include AI-ready fix suggestions
        #[arg(long)]
        suggest_fixes: bool,
    },
    /// Check specific files for i18n key usage
    Check {
        /// Files to check
        files: Vec<PathBuf>,
    },
    /// Fix issues automatically (with approval)
    Fix {
        /// Canonical translation key, for example common:buttons.save
        key: String,

        /// Initial source-locale value; defaults to the canonical key placeholder
        #[arg(long)]
        default_value: Option<String>,

        /// Real target translation in locale=value form; may be repeated
        #[arg(long = "translation")]
        translations: Vec<String>,

        /// Apply the displayed preview. Without this flag no files are changed.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum OutputFormat {
    /// Human-readable terminal output with colors
    Terminal,
    /// JSON format for programmatic consumption
    Json,
    /// Markdown report
    Markdown,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Audit {
            missing_in,
            suggest_fixes,
        } => {
            run_audit(
                &cli.workspace,
                cli.format,
                cli.output,
                missing_in,
                suggest_fixes,
            )
            .await?;
        }
        Commands::Check { files } => {
            run_check(&cli.workspace, files, cli.format, cli.output).await?;
        }
        Commands::Fix {
            key,
            default_value,
            translations,
            apply,
        } => {
            run_fix(&cli.workspace, key, default_value, translations, apply).await?;
        }
    }

    Ok(())
}

async fn run_audit(
    workspace: &Path,
    format: OutputFormat,
    output: Option<PathBuf>,
    missing_in: Option<String>,
    suggest_fixes: bool,
) -> anyhow::Result<()> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")?
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );

    pb.set_message("Loading configuration...");
    let core = Workspace::load(workspace.to_path_buf()).map_err(|failure| {
        anyhow::anyhow!(
            "configuration failed: {}",
            failure
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    let snapshot = core.snapshot();
    pb.set_message("Analyzing React sources and JSON resources...");
    let mut report = (*snapshot.audit).clone();

    // Filter by missing_in if specified
    if let Some(locales_str) = missing_in {
        let locales: HashSet<&str> = locales_str.split(',').map(str::trim).collect();
        report.missing.retain(|m| {
            m.missing_in
                .iter()
                .any(|loc| locales.contains(loc.as_str()))
        });
        // Recalculate summary
        report.summary.missing_translations = report.missing.len();
    }

    pb.finish_and_clear();

    let output_str = match format {
        OutputFormat::Terminal => format_terminal(&report, suggest_fixes),
        OutputFormat::Json => serde_json::to_string_pretty(&report)?,
        OutputFormat::Markdown => format_markdown(&report, suggest_fixes),
    };

    if let Some(output_path) = output {
        std::fs::write(&output_path, output_str)?;
        println!("✓ Report written to {}", output_path.display());
    } else {
        println!("{}", output_str);
    }

    // Exit with error code if issues found
    if report.summary.missing_translations > 0 || report.summary.unused_keys > 0 {
        std::process::exit(1);
    }

    Ok(())
}

async fn run_check(
    workspace: &Path,
    files: Vec<PathBuf>,
    format: OutputFormat,
    output: Option<PathBuf>,
) -> anyhow::Result<()> {
    let core = Workspace::load(workspace.to_path_buf())
        .map_err(|failure| anyhow::anyhow!("configuration failed: {:?}", failure.diagnostics))?;
    let snapshot = core.snapshot();
    let analyzer = ReactSourceAnalyzer::new(AnalyzerConfig {
        default_namespace: snapshot.config.default_namespace.clone(),
        namespace_separator: snapshot.config.namespace_separator,
        key_separator: snapshot.config.key_separator,
    });
    let mut all_keys: Vec<(PathBuf, usize, String)> = Vec::new();

    for file in files {
        let content = std::fs::read_to_string(&file)?;
        let analysis = analyzer.analyze(&file, &content);
        for usage in analysis.usages {
            if let KeyResolution::Static(key) = usage.resolution {
                let line = content[..usage.expression_span.start as usize]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count();
                all_keys.push((file.clone(), line, key.canonical()));
            }
        }
    }

    let mut missing = Vec::new();
    let mut found = Vec::new();

    for (file, line, key) in all_keys {
        let exists = react_i18next_lens::domain::TranslationKey::from_source(
            &key,
            None,
            None,
            &snapshot.config.default_namespace,
            snapshot.config.namespace_separator,
            snapshot.config.key_separator,
        )
        .is_some_and(|identity| {
            snapshot
                .catalog
                .get(&snapshot.config.source_locale, &identity)
                .is_some()
        });
        if exists {
            found.push((file, line, key));
        } else {
            missing.push((file, line, key));
        }
    }

    match format {
        OutputFormat::Terminal => {
            println!("{}", "i18n Key Check Results".bold().underline());
            println!();

            if !missing.is_empty() {
                println!(
                    "{}",
                    format!("❌ Missing Keys ({}):", missing.len()).red().bold()
                );
                for (file, line, key) in &missing {
                    println!(
                        "  {}:{} {}",
                        file.display().to_string().cyan(),
                        line + 1,
                        key.yellow()
                    );
                }
                println!();
            }

            if !found.is_empty() {
                println!(
                    "{}",
                    format!("✓ Found Keys ({})", found.len()).green().bold()
                );
                for (file, line, key) in &found {
                    println!(
                        "  {}:{} {}",
                        file.display().to_string().dimmed(),
                        line + 1,
                        key.dimmed()
                    );
                }
            }

            if !missing.is_empty() {
                std::process::exit(1);
            }
        }
        OutputFormat::Json => {
            let json = serde_json::json!({
                "missing": missing.iter().map(|(f, line, key)| serde_json::json!({
                    "file": f,
                    "line": line + 1,
                    "key": key,
                })).collect::<Vec<_>>(),
                "found": found.iter().map(|(f, line, key)| serde_json::json!({
                    "file": f,
                    "line": line + 1,
                    "key": key,
                })).collect::<Vec<_>>(),
            });
            let output_str = serde_json::to_string_pretty(&json)?;
            if let Some(output_path) = output {
                std::fs::write(&output_path, output_str)?;
            } else {
                println!("{}", output_str);
            }
            if !missing.is_empty() {
                std::process::exit(1);
            }
        }
        OutputFormat::Markdown => {
            let mut md = String::new();
            md.push_str("# i18n Key Check Results\n\n");

            if !missing.is_empty() {
                md.push_str(&format!("## ❌ Missing Keys ({}):\n\n", missing.len()));
                for (file, line, key) in &missing {
                    md.push_str(&format!(
                        "- `{}:{}` - `{}`\n",
                        file.display(),
                        line + 1,
                        key
                    ));
                }
                md.push('\n');
            }

            if !found.is_empty() {
                md.push_str(&format!("## ✓ Found Keys ({}):\n\n", found.len()));
                for (file, line, key) in &found {
                    md.push_str(&format!(
                        "- `{}:{}` - `{}`\n",
                        file.display(),
                        line + 1,
                        key
                    ));
                }
            }

            if let Some(output_path) = output {
                std::fs::write(&output_path, md)?;
            } else {
                println!("{}", md);
            }

            if !missing.is_empty() {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

async fn run_fix(
    workspace: &Path,
    raw_key: String,
    default_value: Option<String>,
    translation_args: Vec<String>,
    apply: bool,
) -> anyhow::Result<()> {
    let core = Workspace::load(workspace.to_path_buf())
        .map_err(|failure| anyhow::anyhow!("configuration failed: {:?}", failure.diagnostics))?;
    let snapshot = core.snapshot();
    let key = TranslationKey::from_source(
        &raw_key,
        None,
        None,
        &snapshot.config.default_namespace,
        snapshot.config.namespace_separator,
        snapshot.config.key_separator,
    )
    .ok_or_else(|| anyhow::anyhow!("invalid translation key: {raw_key}"))?;
    let translations = translation_args
        .into_iter()
        .map(|argument| {
            argument
                .split_once('=')
                .map(|(locale, value)| (locale.to_string(), value.to_string()))
                .ok_or_else(|| anyhow::anyhow!("translation must use locale=value: {argument}"))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;
    let preview = core
        .preview_mutation(&AddMissingKey {
            key,
            default_value,
            translations,
        })
        .map_err(|error| anyhow::anyhow!("mutation preview failed: {error:?}"))?;

    println!(
        "Mutation preview (generation {}):",
        preview.generation.value()
    );
    for edit in &preview.edits {
        println!("\n--- {} (before)\n{}", edit.file.display(), edit.before);
        println!("+++ {} (after)\n{}", edit.file.display(), edit.after);
    }
    if apply {
        core.apply_mutation(&preview)
            .map_err(|error| anyhow::anyhow!("mutation apply failed: {error:?}"))?;
        println!("\nApplied {} file edit(s).", preview.edits.len());
    } else {
        println!("\nPreview only. Re-run with --apply to write these edits.");
    }
    Ok(())
}

fn format_terminal(report: &react_i18next_lens::audit::AuditReport, suggest_fixes: bool) -> String {
    let mut output = String::new();

    output.push_str(&format!(
        "{}\n\n",
        "╔══════════════════════════════════════╗".blue().bold()
    ));
    output.push_str(&format!(
        "{}\n",
        "║      i18n Audit Report               ║".blue().bold()
    ));
    output.push_str(&format!(
        "{}\n\n",
        "╚══════════════════════════════════════╝".blue().bold()
    ));

    // Summary
    output.push_str(&"Summary\n".bold().underline().to_string());
    output.push_str(&format!(
        "  Total Keys:        {}\n",
        report.summary.total_keys.to_string().cyan()
    ));
    output.push_str(&format!(
        "  Total Locales:     {}\n",
        report.summary.total_locales.to_string().cyan()
    ));

    let missing_str = if report.summary.missing_translations > 0 {
        report.summary.missing_translations.to_string().red().bold()
    } else {
        report.summary.missing_translations.to_string().green()
    };
    output.push_str(&format!("  Missing Translations: {}\n", missing_str));

    let unused_str = if report.summary.unused_keys > 0 {
        report.summary.unused_keys.to_string().yellow().bold()
    } else {
        report.summary.unused_keys.to_string().green()
    };
    output.push_str(&format!("  Unused Keys:       {}\n", unused_str));

    if report.summary.placeholder_mismatches > 0 {
        output.push_str(&format!(
            "  Placeholder Issues: {}\n",
            report
                .summary
                .placeholder_mismatches
                .to_string()
                .red()
                .bold()
        ));
    }
    output.push('\n');

    // Missing translations
    if !report.missing.is_empty() {
        output.push_str(&format!(
            "{}",
            "Missing Translations\n".red().bold().underline()
        ));
        for item in &report.missing {
            output.push_str(&format!("  {} {}\n", "•".red(), item.key.yellow()));
            output.push_str(&format!(
                "    Source ({}): {}\n",
                item.source_locale,
                item.source_value.dimmed()
            ));
            output.push_str(&format!(
                "    Missing in: {}\n",
                item.missing_in.join(", ").red()
            ));

            if !item.used_in.is_empty() {
                output.push_str("    Used in:\n");
                for usage in &item.used_in {
                    output.push_str(&format!(
                        "      - {}:{}\n",
                        usage.file.display().to_string().dimmed(),
                        usage.line + 1
                    ));
                }
            }

            if suggest_fixes {
                if let Some(sugg) = item.suggestion.as_ref() {
                    output.push_str(&format!(
                        "    {} {}\n",
                        "→".green(),
                        "Suggestion:".green().bold()
                    ));
                    output.push_str(&format!("      Action: {}\n", sugg.action.green()));
                    if !sugg.files_to_edit.is_empty() {
                        output.push_str("      Files to edit:\n");
                        for f in &sugg.files_to_edit {
                            output.push_str(&format!(
                                "        - {}\n",
                                f.display().to_string().green()
                            ));
                        }
                    }
                }
            }
            output.push('\n');
        }
    }

    // Unused keys
    if !report.unused.is_empty() {
        output.push_str(&"Unused Keys\n".yellow().bold().underline().to_string());
        for item in &report.unused {
            output.push_str(&format!("  {} {}\n", "•".yellow(), item.key.dimmed()));
            output.push_str(&format!(
                "    Defined in: {}:{}\n",
                item.defined_in.file_path.display().to_string().dimmed(),
                item.defined_in.line
            ));
            output.push('\n');
        }
    }

    // Placeholder issues
    if !report.placeholder_issues.is_empty() {
        output.push_str(&format!(
            "{}",
            "Placeholder Issues\n".red().bold().underline()
        ));
        for item in &report.placeholder_issues {
            output.push_str(&format!("  {} {}\n", "•".red(), item.key.yellow()));
            output.push_str(&format!(
                "    Expected placeholders: {}\n",
                item.expected_placeholders.join(", ").cyan()
            ));
            output.push_str("    Mismatched locales:\n");
            for (locale, value) in &item.locale_values {
                output.push_str(&format!("      {}: {}\n", locale.red(), value));
            }
            output.push('\n');
        }
    }

    if report.missing.is_empty() && report.unused.is_empty() && report.placeholder_issues.is_empty()
    {
        output.push_str(&format!("{}\n", "✓ All i18n checks passed!".green().bold()));
    }

    output
}

fn format_markdown(report: &react_i18next_lens::audit::AuditReport, suggest_fixes: bool) -> String {
    let mut md = String::new();

    md.push_str("# i18n Audit Report\n\n");

    // Summary
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Count |\n");
    md.push_str("|--------|-------|\n");
    md.push_str(&format!("| Total Keys | {} |\n", report.summary.total_keys));
    md.push_str(&format!(
        "| Total Locales | {} |\n",
        report.summary.total_locales
    ));

    let missing_badge = if report.summary.missing_translations > 0 {
        format!("**{}** ⚠️", report.summary.missing_translations)
    } else {
        format!("{} ✓", report.summary.missing_translations)
    };
    md.push_str(&format!("| Missing Translations | {} |\n", missing_badge));

    let unused_badge = if report.summary.unused_keys > 0 {
        format!("**{}** ⚠️", report.summary.unused_keys)
    } else {
        format!("{} ✓", report.summary.unused_keys)
    };
    md.push_str(&format!("| Unused Keys | {} |\n", unused_badge));

    if report.summary.placeholder_mismatches > 0 {
        md.push_str(&format!(
            "| Placeholder Issues | **{}** ⚠️ |\n",
            report.summary.placeholder_mismatches
        ));
    }
    md.push('\n');

    // Missing translations
    if !report.missing.is_empty() {
        md.push_str("## Missing Translations\n\n");
        for item in &report.missing {
            md.push_str(&format!("### `{}`\n\n", item.key));
            md.push_str(&format!(
                "- **Source ({}):** {}\n",
                item.source_locale, item.source_value
            ));
            md.push_str(&format!(
                "- **Missing in:** `{}`\n",
                item.missing_in.join("`, `")
            ));

            if !item.used_in.is_empty() {
                md.push_str("- **Used in:**\n");
                for usage in &item.used_in {
                    md.push_str(&format!(
                        "  - `{}:{}`\n",
                        usage.file.display(),
                        usage.line + 1
                    ));
                }
            }

            if suggest_fixes {
                if let Some(sugg) = item.suggestion.as_ref() {
                    md.push_str("\n**Suggestion:**\n");
                    md.push_str(&format!("- Action: `{}`\n", sugg.action));
                    if !sugg.files_to_edit.is_empty() {
                        md.push_str("- Files to edit:\n");
                        for f in &sugg.files_to_edit {
                            md.push_str(&format!("  - `{}`\n", f.display()));
                        }
                    }
                }
            }
            md.push('\n');
        }
    }

    // Unused keys
    if !report.unused.is_empty() {
        md.push_str("## Unused Keys\n\n");
        for item in &report.unused {
            md.push_str(&format!(
                "- `{}` - defined in `{}:{}`\n",
                item.key,
                item.defined_in.file_path.display(),
                item.defined_in.line
            ));
        }
        md.push('\n');
    }

    // Placeholder issues
    if !report.placeholder_issues.is_empty() {
        md.push_str("## Placeholder Issues\n\n");
        for item in &report.placeholder_issues {
            md.push_str(&format!("### `{}`\n\n", item.key));
            md.push_str(&format!(
                "- **Expected placeholders:** `{}`\n",
                item.expected_placeholders.join("`, `")
            ));
            md.push_str("- **Mismatched locales:**\n");
            for (locale, value) in &item.locale_values {
                md.push_str(&format!("  - `{}`: `{}`\n", locale, value));
            }
            md.push('\n');
        }
    }

    if report.missing.is_empty() && report.unused.is_empty() && report.placeholder_issues.is_empty()
    {
        md.push_str("## ✓ All i18n checks passed!\n");
    }

    md
}
