//! Cargo subcommand to format and normalize Cargo.toml files according to
//! workspace standards.
//!
//! This tool enforces:
//! 1. All dependency versions at workspace level
//! 2. Internal dependencies use { workspace = true }
//! 3. All dependencies sorted alphabetically
//! 4. Consistent [package] section format

use std::collections::BTreeMap;
use std::path::{
    Path,
    PathBuf,
};

use anyhow::{
    Context,
    Result,
};
use cargo_plugin_utils::ProgressLogger;
use clap::Parser;
use toml_edit::{
    DocumentMut,
    InlineTable,
    Item,
    Table,
    Value,
};

#[derive(Parser, Debug)]
#[command(
    name = "cargo-fmt-toml",
    about = "Format and normalize Cargo.toml files according to workspace standards",
    bin_name = "cargo",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Parser, Debug)]
enum Command {
    #[command(name = "fmt-toml")]
    FmtToml(FmtArgs),
}

#[derive(Parser, Debug)]
struct FmtArgs {
    /// Show what would be changed without modifying files
    #[arg(long)]
    dry_run: bool,

    /// Check if files need formatting (exit code 1 if changes needed)
    #[arg(long)]
    check: bool,

    /// Path to workspace root
    #[arg(long, default_value = ".")]
    workspace_path: PathBuf,

    /// Suppress output when there are no changes
    #[arg(long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::FmtToml(args)) => fmt_toml(args),
        None => {
            // When invoked without a subcommand, show help
            use clap::CommandFactory;
            Cli::command().print_help()?;
            Ok(())
        }
    }
}

fn fmt_toml(args: FmtArgs) -> Result<()> {
    let mut logger = ProgressLogger::new(args.quiet);

    // Use cargo_metadata to get all workspace packages
    let packages =
        cargo_plugin_utils::get_workspace_packages(Some(&args.workspace_path.join("Cargo.toml")))?;

    let crate_manifests: Vec<PathBuf> = packages
        .iter()
        .map(|pkg| pkg.manifest_path.as_std_path().to_path_buf())
        .collect();

    let mut total_changes = 0;
    let mut files_changed = 0;

    logger.set_progress(crate_manifests.len() as u64);
    logger.set_message("🔍 Formatting Cargo.toml files");

    for manifest_path in &crate_manifests {
        logger.inc();
        let changes = format_manifest(manifest_path, &args, &mut logger)?;
        if changes > 0 {
            total_changes += changes;
            files_changed += 1;
        }
    }
    logger.finish();

    // In quiet mode, show nothing. Otherwise show summary.
    if !args.quiet {
        if total_changes > 0 {
            logger.println("✨ Complete!");
            if args.dry_run || args.check {
                logger.println(&format!("   {} files need formatting", files_changed));
                logger.println(&format!("   {} total changes needed", total_changes));
                if args.check {
                    std::process::exit(1);
                } else {
                    logger.println("   Run without --dry-run to apply changes");
                }
            } else {
                logger.println(&format!("   Formatted {} files", files_changed));
                logger.println(&format!("   Made {} changes", total_changes));
            }
        } else {
            logger.println("✨ All files are properly formatted");
        }
    } else if args.check && total_changes > 0 {
        // In quiet + check mode, still exit with error code
        std::process::exit(1);
    }

    Ok(())
}

fn format_manifest(
    manifest_path: &Path,
    args: &FmtArgs,
    logger: &mut ProgressLogger,
) -> Result<usize> {
    let content = std::fs::read_to_string(manifest_path)
        .context(format!("Failed to read {:?}", manifest_path))?;

    let mut doc = content
        .parse::<DocumentMut>()
        .context(format!("Failed to parse {:?}", manifest_path))?;

    let mut changes = 0;

    // 1. Collapse nested tables into inline entries where appropriate
    changes += collapse_nested_tables(&mut doc, logger)?;

    // 2. Reorder sections in the document
    changes += reorder_sections(&mut doc, logger)?;

    // 3. Format [package] section
    changes += format_package_section(&mut doc, logger)?;

    // 4. Sort all dependency sections
    changes += sort_dependencies(&mut doc, "dependencies", logger)?;
    changes += sort_dependencies(&mut doc, "dev-dependencies", logger)?;
    changes += sort_dependencies(&mut doc, "build-dependencies", logger)?;

    // 5. Sort target-specific dependencies
    if let Some(target_table) = doc.get_mut("target").and_then(|t| t.as_table_mut()) {
        for (_target_name, target_config) in target_table.iter_mut() {
            if target_config.get("dependencies").is_some()
                && let Some(deps_table) = target_config
                    .get_mut("dependencies")
                    .and_then(|d| d.as_table_mut())
            {
                let collapsed = collapse_table_entries(deps_table);
                if collapsed > 0 {
                    deps_table.set_implicit(false);
                    changes += collapsed;
                }
                changes += sort_table_in_place(deps_table, logger)?;
            }
        }
    }

    if changes > 0 {
        logger.println(&format!("\n📦 {}", manifest_path.display()));

        if args.dry_run || args.check {
            logger.println(&format!("   Would format with {} changes", changes));
        } else {
            std::fs::write(manifest_path, doc.to_string())
                .context(format!("Failed to write {:?}", manifest_path))?;
            logger.println(&format!("   💾 Formatted with {} changes", changes));
        }
    }

    Ok(changes)
}

fn collapse_nested_tables(doc: &mut DocumentMut, logger: &mut ProgressLogger) -> Result<usize> {
    let mut changes = 0;

    if let Some(package) = doc.get_mut("package").and_then(|p| p.as_table_mut()) {
        let collapsed = collapse_table_entries(package);
        if collapsed > 0 {
            changes += collapsed;
        }
    }

    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = doc.get_mut(section).and_then(|d| d.as_table_mut()) {
            let collapsed = collapse_table_entries(deps);
            if collapsed > 0 {
                deps.set_implicit(false);
                changes += collapsed;
            }
        }
    }

    if let Some(target_table) = doc.get_mut("target").and_then(|t| t.as_table_mut()) {
        for (_target_name, target_config) in target_table.iter_mut() {
            if let Some(deps_table) = target_config
                .get_mut("dependencies")
                .and_then(|d| d.as_table_mut())
            {
                let collapsed = collapse_table_entries(deps_table);
                if collapsed > 0 {
                    deps_table.set_implicit(false);
                    changes += collapsed;
                }
            }
        }
    }

    if changes > 0 {
        logger.println("   ✓ Collapsed nested tables into inline entries");
    }

    Ok(changes)
}

fn collapse_table_entries(table: &mut Table) -> usize {
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let mut replacements: Vec<(String, InlineTable)> = Vec::new();

    for key in &keys {
        let Some(Item::Table(inner)) = table.get(key) else {
            continue;
        };

        if inner.is_dotted() {
            continue;
        }

        let mut inline = InlineTable::new();
        let mut convertible = true;

        for (child_key, child_item) in inner.iter() {
            if let Some(value) = child_item.as_value() {
                inline.insert(child_key, value.clone());
            } else {
                convertible = false;
                break;
            }
        }

        if convertible {
            replacements.push((key.clone(), inline));
        }
    }

    let mut changes = 0;
    for (key, inline) in replacements {
        if let Some(item) = table.get_mut(&key) {
            *item = Item::Value(Value::InlineTable(inline));
            changes += 1;
        } else {
            table.insert(&key, Item::Value(Value::InlineTable(inline)));
            changes += 1;
        }
    }

    changes
}

fn reorder_sections(doc: &mut DocumentMut, logger: &mut ProgressLogger) -> Result<usize> {
    // Define the desired section order
    let section_order = vec![
        "package",
        "lib",
        "bin",
        "test",
        "bench",
        "example",
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "target",
        "features",
    ];

    // Get current top-level keys
    let current_keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();

    // Build expected order: ordered sections first, then any extra sections
    let mut expected_keys = Vec::new();
    for section in &section_order {
        if current_keys.contains(&section.to_string()) {
            expected_keys.push(section.to_string());
        }
    }

    // Add any keys not in section_order at the end
    for key in &current_keys {
        if !section_order.contains(&key.as_str()) {
            expected_keys.push(key.clone());
        }
    }

    // Check if reordering is needed
    if current_keys == expected_keys {
        return Ok(0);
    }

    // Manually reconstruct the document string in the desired order.
    // This preserves all formatting including inline tables.
    //
    // Dotted section headers like [workspace.package] belong to their
    // top-level parent key (workspace). We group them together so that
    // the rebuild loop emits all sub-sections with their parent.
    let original_str = doc.to_string();
    let mut section_strings: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // Split the document into sections manually by finding section
    // headers. Each top-level key accumulates its own content plus
    // any dotted sub-sections (e.g. [workspace.package]).
    let mut current_content = String::new();
    let mut current_top_level_key = String::new();

    for line in original_str.lines() {
        let trimmed = line.trim();

        let header_name = if trimmed.starts_with("[[") {
            // Potential array-of-tables header like [[bin]].
            // Real headers end with ]] optionally followed by
            // whitespace or a comment — nothing else.
            trimmed.find("]]").and_then(|end| {
                let after = trimmed[end + 2..].trim_start();
                if after.is_empty() || after.starts_with('#') {
                    Some(trimmed[2..end].to_string())
                } else {
                    None // Value line, e.g. [[1, 2], [3, 4]]
                }
            })
        } else if trimmed.starts_with('[') {
            // Potential standard table header like [package].
            // Real headers end with ] optionally followed by
            // whitespace or a comment — nothing else.
            trimmed.find(']').and_then(|end| {
                let after = trimmed[end + 1..].trim_start();
                if after.is_empty() || after.starts_with('#') {
                    Some(trimmed[1..end].to_string())
                } else {
                    None // Value line, e.g. ["a", "b"]
                }
            })
        } else {
            None
        };

        if let Some(name) = header_name {
            // Determine the top-level key for this header
            let top_level = name.split('.').next().unwrap_or(&name).to_string();

            if top_level != current_top_level_key {
                // Starting a new top-level group — save the previous one
                if !current_top_level_key.is_empty() {
                    section_strings
                        .entry(current_top_level_key.clone())
                        .and_modify(|existing| existing.push_str(&current_content))
                        .or_insert_with(|| current_content.clone());
                }
                current_top_level_key = top_level;
                current_content = format!("{}\n", line);
            } else {
                // Same top-level group (e.g. [workspace.package] after
                // [workspace]) — append to current content
                current_content.push_str(line);
                current_content.push('\n');
            }
        } else {
            // Regular content line — append to current section
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Don't forget the last section
    if !current_top_level_key.is_empty() {
        section_strings
            .entry(current_top_level_key)
            .and_modify(|existing| existing.push_str(&current_content))
            .or_insert(current_content);
    }

    // Rebuild in the desired order
    let mut new_content = String::new();
    for key in &expected_keys {
        if let Some(section_str) = section_strings.get(key) {
            if !new_content.is_empty() && !new_content.ends_with("\n\n") {
                new_content.push('\n');
            }
            new_content.push_str(section_str);
        }
    }

    // Parse the reordered content back
    *doc = new_content
        .parse::<DocumentMut>()
        .context("Failed to parse reordered document")?;

    logger.println("   ✓ Reordered sections");

    Ok(1)
}

fn format_package_section(doc: &mut DocumentMut, logger: &mut ProgressLogger) -> Result<usize> {
    let mut changes = 0;

    if let Some(package) = doc.get_mut("package").and_then(|p| p.as_table_mut()) {
        // Define the desired order
        let desired_order = vec![
            "name",
            "description",
            "version",
            "edition",
            "license-file",
            "authors",
            "rust-version",
            "readme",
        ];

        // Check if order is correct
        let current_keys: Vec<String> = package.iter().map(|(k, _)| k.to_string()).collect();
        let mut expected_keys = Vec::new();
        for key in &desired_order {
            if package.contains_key(key) {
                expected_keys.push(key.to_string());
            }
        }

        // Add any keys that aren't in desired_order at the end
        for key in &current_keys {
            if !desired_order.contains(&key.as_str()) {
                expected_keys.push(key.clone());
            }
        }

        if current_keys != expected_keys {
            // Need to reorder - collect all entries first
            let keys_to_collect: Vec<String> = package.iter().map(|(k, _)| k.to_string()).collect();
            let mut entries = BTreeMap::new();
            for key in keys_to_collect {
                if let Some(item) = package.remove(&key) {
                    entries.insert(key, item);
                }
            }

            // Re-insert in desired order
            for key in &expected_keys {
                if let Some(item) = entries.remove(key) {
                    package.insert(key, item);
                }
            }

            logger.println("   ✓ Reordered [package] section");
            changes += 1;
        }
    }

    Ok(changes)
}

fn sort_dependencies(
    doc: &mut DocumentMut,
    section: &str,
    logger: &mut ProgressLogger,
) -> Result<usize> {
    if let Some(deps) = doc.get_mut(section).and_then(|d| d.as_table_mut()) {
        sort_table_in_place(deps, logger)
    } else {
        Ok(0)
    }
}

fn sort_table_in_place(table: &mut Table, logger: &mut ProgressLogger) -> Result<usize> {
    let current_keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let mut sorted_keys = current_keys.clone();
    sorted_keys.sort();

    if current_keys != sorted_keys {
        // Need to reorder
        let mut entries = BTreeMap::new();
        for key in &current_keys {
            if let Some(item) = table.remove(key) {
                entries.insert(key.clone(), item);
            }
        }

        // Re-insert in sorted order
        for key in &sorted_keys {
            if let Some(item) = entries.remove(key) {
                table.insert(key, item);
            }
        }

        logger.println("   ✓ Sorted dependencies alphabetically");
        return Ok(1);
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that runs `reorder_sections` on the given TOML string
    /// and returns the resulting TOML string.
    fn reorder(input: &str) -> String {
        let mut doc = input.parse::<DocumentMut>().expect("valid TOML");
        let mut logger = ProgressLogger::new(true);
        reorder_sections(&mut doc, &mut logger).expect("reorder succeeded");
        doc.to_string()
    }

    #[test]
    fn workspace_dotted_sections_preserved() {
        let input = "\
[package]
name = \"test-workspace\"
version = \"0.0.0\"

[workspace]
members = [\"crate-a\"]
resolver = \"3\"

[profile]

[workspace.package]
rust-version = \"1.93.0\"
edition = \"2024\"

[workspace.dependencies]
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = { version = \"1.0\" }
";
        let result = reorder(input);

        // All dotted workspace sections must be present
        assert!(
            result.contains("[workspace.package]"),
            "missing [workspace.package] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.dependencies]"),
            "missing [workspace.dependencies] in:\n{result}"
        );
        assert!(
            result.contains("rust-version"),
            "missing rust-version field in:\n{result}"
        );
        assert!(
            result.contains("serde"),
            "missing serde dependency in:\n{result}"
        );
        assert!(
            result.contains("tokio"),
            "missing tokio dependency in:\n{result}"
        );
        assert!(
            result.contains("[profile]"),
            "missing [profile] in:\n{result}"
        );
    }

    #[test]
    fn sections_not_in_order_list_are_preserved() {
        let input = "\
[package]
name = \"test\"

[lints]
workspace = true

[dependencies]
serde = \"1.0\"
";
        let result = reorder(input);

        assert!(
            result.contains("[lints]"),
            "missing [lints] section in:\n{result}"
        );
        assert!(
            result.contains("workspace = true"),
            "missing lints content in:\n{result}"
        );
    }

    #[test]
    fn no_truncation_with_many_dotted_sections() {
        let input = "\
[package]
name = \"big-workspace\"
version = \"0.0.0\"

[workspace]
members = [\"a\", \"b\", \"c\"]
resolver = \"3\"

[profile.release]
opt-level = 3

[profile.dev]
opt-level = 0

[workspace.package]
edition = \"2024\"
license = \"MIT\"

[workspace.dependencies]
anyhow = \"1.0\"
clap = { version = \"4.0\", features = [\"derive\"] }
serde = { version = \"1.0\" }
tokio = { version = \"1.0\" }
";
        let result = reorder(input);

        // Verify nothing is lost
        assert!(
            result.contains("[workspace.package]"),
            "missing [workspace.package]:\n{result}"
        );
        assert!(
            result.contains("[workspace.dependencies]"),
            "missing [workspace.dependencies]:\n{result}"
        );
        assert!(
            result.contains("[profile.release]"),
            "missing [profile.release]:\n{result}"
        );
        assert!(
            result.contains("[profile.dev]"),
            "missing [profile.dev]:\n{result}"
        );
        assert!(result.contains("anyhow"), "missing anyhow dep:\n{result}");
        assert!(result.contains("tokio"), "missing tokio dep:\n{result}");
        assert!(
            result.contains("edition = \"2024\""),
            "missing edition field:\n{result}"
        );
    }

    #[test]
    fn lints_clippy_with_inline_priority_preserved() {
        // Reproduces the reported bug: a [lints.clippy] section with
        // entries like `disallowed_types = { level = "warn", priority = 1 }`
        // was causing "Failed to parse reordered document" errors.
        // The line-based parser must not misidentify value lines
        // containing brackets as section headers.
        let input = "\
[lints.clippy]
disallowed_types = { level = \"warn\", priority = 1 }
disallowed-names = { level = \"warn\", priority = -1 }

[package]
name = \"test-crate\"
version = \"0.1.0\"

[dependencies]
serde = \"1.0\"
";
        let result = reorder(input);

        assert!(
            result.contains("[lints.clippy]"),
            "missing [lints.clippy] in:\n{result}"
        );
        assert!(
            result.contains("priority = 1"),
            "missing priority = 1 in:\n{result}"
        );
        assert!(
            result.contains("priority = -1"),
            "missing priority = -1 in:\n{result}"
        );
        assert!(
            result.contains("[package]"),
            "missing [package] in:\n{result}"
        );
        assert!(
            result.contains("[dependencies]"),
            "missing [dependencies] in:\n{result}"
        );
    }

    #[test]
    fn multiline_arrays_not_misidentified_as_headers() {
        // Value lines starting with [ (array elements, nested arrays)
        // must not be misidentified as section headers.
        let input = "\
[package]
name = \"test\"
categories = [
    \"command-line-utilities\",
    \"development-tools\",
]

[features]
default = [\"std\"]

[dependencies]
serde = \"1.0\"
";
        let result = reorder(input);

        assert!(
            result.contains("categories"),
            "missing categories in:\n{result}"
        );
        assert!(
            result.contains("command-line-utilities"),
            "missing array element in:\n{result}"
        );
        assert!(
            result.contains("[features]"),
            "missing [features] in:\n{result}"
        );
    }

    #[test]
    fn nested_array_values_not_misidentified_as_headers() {
        // Nested arrays like [[1, 2], [3, 4]] should not be treated
        // as [[array-of-tables]] headers.
        let input = "\
[package]
name = \"test\"

[metadata]
matrix = [
    [1, 2],
    [3, 4],
]

[dependencies]
serde = \"1.0\"
";
        let result = reorder(input);

        assert!(
            result.contains("[metadata]"),
            "missing [metadata] in:\n{result}"
        );
        assert!(
            result.contains("[1, 2]"),
            "missing nested array [1, 2] in:\n{result}"
        );
        assert!(
            result.contains("[3, 4]"),
            "missing nested array [3, 4] in:\n{result}"
        );
    }

    #[test]
    fn multiline_feature_arrays_with_brackets() {
        // Feature arrays with entries in brackets on their own line
        // must not be misidentified as section headers. This
        // reproduces the reported "invalid multi-line basic string"
        // error when inline tables get expanded to multi-line.
        let input = "\
[package]
name = \"test\"
keywords = [
    \"cargo\",
    \"toml\",
]

[features]
full = [
    \"derive\",
    \"std\",
]

[dependencies]
serde = \"1.0\"
";
        let result = reorder(input);

        assert!(
            result.contains("[features]"),
            "missing [features] in:\n{result}"
        );
        assert!(
            result.contains("\"derive\""),
            "missing derive feature in:\n{result}"
        );
        assert!(
            result.contains("keywords"),
            "missing keywords in:\n{result}"
        );
    }
}
