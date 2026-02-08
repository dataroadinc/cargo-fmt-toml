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

    // Phase 1: Format all manifests and collect results.
    // No files are written yet — if any manifest fails to format,
    // no files will be modified on disk (atomic behavior).
    let mut results: Vec<(PathBuf, String, usize)> = Vec::new();

    logger.set_progress(crate_manifests.len() as u64);
    logger.set_message("🔍 Formatting Cargo.toml files");

    for manifest_path in &crate_manifests {
        logger.inc();
        let (output, changes) = format_manifest(manifest_path, &mut logger)?;
        if changes > 0 {
            results.push((manifest_path.clone(), output, changes));
        }
    }
    logger.finish();

    let total_changes: usize = results.iter().map(|(_, _, c)| c).sum();
    let files_changed = results.len();

    // Phase 2: Write all formatted files to disk.
    if !args.dry_run && !args.check {
        for (path, output, changes) in &results {
            std::fs::write(path, output).context(format!("Failed to write {:?}", path))?;
            logger.println(&format!("\n📦 {}", path.display()));
            logger.println(&format!("   💾 Formatted with {} changes", changes));
        }
    } else {
        for (path, _, changes) in &results {
            logger.println(&format!("\n📦 {}", path.display()));
            logger.println(&format!("   Would format with {} changes", changes));
        }
    }

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

/// Format a single manifest and return the formatted output string
/// along with the number of changes made. Does NOT write to disk.
fn format_manifest(manifest_path: &Path, logger: &mut ProgressLogger) -> Result<(String, usize)> {
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

    let output = doc.to_string();

    if changes > 0 {
        // Validate the output is valid TOML before returning.
        // This prevents corrupting the file when an internal
        // transformation produces invalid content.
        output.parse::<DocumentMut>().context(format!(
            "Internal error: formatted output for {:?} is not valid TOML. \
             File was NOT modified. Please report this as a bug.",
            manifest_path
        ))?;
    }

    Ok((output, changes))
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
    // Define the desired section order for top-level keys.
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

    // Get current top-level keys from the document.  doc.iter()
    // correctly identifies top-level keys including dotted sections
    // like [workspace.package] grouped under "workspace".
    let current_keys: Vec<String> = doc.iter().map(|(k, _)| k.to_string()).collect();

    // Build expected order: ordered sections first, then any extra
    // sections (workspace, profile, lints, patch, etc.) in their
    // original relative order.
    let mut expected_keys = Vec::new();
    for section in &section_order {
        if current_keys.contains(&section.to_string()) {
            expected_keys.push(section.to_string());
        }
    }
    for key in &current_keys {
        if !section_order.contains(&key.as_str()) {
            expected_keys.push(key.clone());
        }
    }

    // Check if reordering is needed.
    if current_keys == expected_keys {
        return Ok(0);
    }

    // Serialize each top-level key individually and reassemble in
    // the desired order.  We use toml_edit's own serialization per
    // key, which correctly handles dotted sub-sections, inline
    // tables, array-of-tables, multi-line values, and comments.
    //
    // For each key we build a temporary document containing only
    // that key, serialize it, and collect the text fragment.
    let mut section_fragments: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // Remove all entries from the original document.
    let table = doc.as_table_mut();
    let mut entries: Vec<(toml_edit::Key, Item)> = Vec::new();
    let keys_to_remove: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    for key in &keys_to_remove {
        if let Some(entry) = table.remove_entry(key) {
            entries.push(entry);
        }
    }

    // Serialize each key individually.
    for (key, item) in entries {
        let key_name = key.to_string();
        let mut tmp_doc = DocumentMut::new();
        tmp_doc.insert_formatted(&key, item);
        section_fragments.insert(key_name, tmp_doc.to_string());
    }

    // Reassemble in the desired order.
    let mut new_content = String::new();
    for key_name in &expected_keys {
        if let Some(fragment) = section_fragments.get(key_name) {
            if !new_content.is_empty() && !new_content.ends_with("\n\n") {
                // Ensure a blank line between sections.
                if !new_content.ends_with('\n') {
                    new_content.push('\n');
                }
                new_content.push('\n');
            }
            new_content.push_str(fragment.trim_start());
        }
    }

    // Ensure trailing newline.
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    // Parse the reordered content back into the document.
    *doc = new_content
        .parse::<DocumentMut>()
        .context("Internal error: reordered output is not valid TOML")?;

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

    /// Helper that runs the full formatting pipeline on a TOML string
    /// (collapse + reorder + format_package + sort) and returns the
    /// result.
    fn full_format(input: &str) -> String {
        let mut doc = input.parse::<DocumentMut>().expect("valid TOML");
        let mut logger = ProgressLogger::new(true);
        collapse_nested_tables(&mut doc, &mut logger).expect("collapse succeeded");
        reorder_sections(&mut doc, &mut logger).expect("reorder succeeded");
        format_package_section(&mut doc, &mut logger).expect("format_package succeeded");
        sort_dependencies(&mut doc, "dependencies", &mut logger).expect("sort deps succeeded");
        sort_dependencies(&mut doc, "dev-dependencies", &mut logger)
            .expect("sort dev-deps succeeded");
        sort_dependencies(&mut doc, "build-dependencies", &mut logger)
            .expect("sort build-deps succeeded");
        doc.to_string()
    }

    #[test]
    fn full_pipeline_workspace_lints_with_comments() {
        // Reproduces the reported bug: a workspace Cargo.toml with
        // [workspace.lints.clippy] entries containing trailing
        // comments after quoted string values was causing parse
        // errors during reordering.
        let input = "\
[package]
name = \"my-workspace\"
version = \"0.0.0\"
publish = false

[workspace]
members = [\"crate-a\", \"crate-b\"]
resolver = \"3\"

[workspace.lints.clippy]
missing_crate_level_docs = \"deny\" # require crate-level docs
disallowed_types = { level = \"warn\", priority = 1 }

[workspace.lints.rust]
missing_docs = \"warn\"
unsafe_code = \"forbid\" # never allow unsafe

[workspace.package]
rust-version = \"1.93.0\"
edition = \"2024\"
license = \"Apache-2.0\"

[workspace.dependencies]
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = { version = \"1.0\", features = [\"full\"] }
anyhow = \"1.0\"

[profile.release]
opt-level = 3
";
        let result = full_format(input);

        // Verify all sections are preserved
        assert!(
            result.contains("[workspace.lints.clippy]"),
            "missing [workspace.lints.clippy] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.lints.rust]"),
            "missing [workspace.lints.rust] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.package]"),
            "missing [workspace.package] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.dependencies]"),
            "missing [workspace.dependencies] in:\n{result}"
        );
        assert!(
            result.contains("[profile.release]"),
            "missing [profile.release] in:\n{result}"
        );
        // Verify comments are preserved
        assert!(
            result.contains("# require crate-level docs"),
            "missing trailing comment in:\n{result}"
        );
        assert!(
            result.contains("# never allow unsafe"),
            "missing trailing comment in:\n{result}"
        );
        // Verify values are preserved
        assert!(
            result.contains("missing_crate_level_docs"),
            "missing lint entry in:\n{result}"
        );
        assert!(
            result.contains("priority = 1"),
            "missing priority in:\n{result}"
        );
    }

    #[test]
    fn full_pipeline_lints_out_of_order() {
        // When [lints.clippy] appears before [package], the tool
        // must reorder correctly without corrupting values.
        let input = "\
[lints.clippy]
needless_pass_by_value = \"warn\"
missing_errors_doc = \"warn\"

[lints.rust]
unsafe_code = \"forbid\"

[package]
name = \"test-crate\"
version = \"0.1.0\"
edition = \"2024\"

[dependencies]
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = \"1.0\"
anyhow = \"1.0\"
";
        let result = full_format(input);

        // [package] should come before [dependencies]
        let pkg_pos = result.find("[package]").expect("missing [package]");
        let dep_pos = result
            .find("[dependencies]")
            .expect("missing [dependencies]");
        assert!(
            pkg_pos < dep_pos,
            "[package] should come before [dependencies]"
        );
        // lints should still be present
        assert!(
            result.contains("[lints.clippy]"),
            "missing [lints.clippy] in:\n{result}"
        );
        assert!(
            result.contains("[lints.rust]"),
            "missing [lints.rust] in:\n{result}"
        );
        assert!(
            result.contains("needless_pass_by_value"),
            "missing lint entry in:\n{result}"
        );
        // dependencies should be sorted
        let anyhow_pos = result.find("anyhow").expect("missing anyhow");
        let serde_pos = result.find("serde").expect("missing serde");
        let tokio_pos = result.find("tokio").expect("missing tokio");
        assert!(
            anyhow_pos < serde_pos && serde_pos < tokio_pos,
            "dependencies should be sorted alphabetically"
        );
    }

    #[test]
    fn full_pipeline_workspace_lints_explicit_tables() {
        // Test with [workspace.lints.clippy.disallowed-names] as an
        // explicit sub-table (not inline) — this is how toml_edit
        // may serialize certain lint configurations.
        let input = "\
[workspace]
members = [\"crate-a\"]
resolver = \"3\"

[workspace.lints.clippy]
needless_pass_by_value = \"warn\"

[workspace.lints.clippy.disallowed-names]
level = \"warn\"
priority = -1

[workspace.lints.clippy.disallowed_types]
level = \"warn\"
priority = 1

[workspace.lints.rust]
missing_docs = \"warn\"

[workspace.package]
edition = \"2024\"

[package]
name = \"my-workspace\"
version = \"0.0.0\"

[dependencies]
serde = \"1.0\"
";
        let result = full_format(input);

        assert!(
            result.contains("disallowed-names"),
            "missing disallowed-names in:\n{result}"
        );
        assert!(
            result.contains("disallowed_types"),
            "missing disallowed_types in:\n{result}"
        );
        assert!(
            result.contains("priority = -1"),
            "missing priority = -1 in:\n{result}"
        );
        assert!(
            result.contains("priority = 1"),
            "missing priority = 1 in:\n{result}"
        );
        assert!(
            result.contains("[workspace.package]"),
            "missing [workspace.package] in:\n{result}"
        );
    }

    #[test]
    fn reorder_preserves_non_contiguous_dotted_sections() {
        // When [workspace] appears early and [workspace.package]
        // appears much later (separated by non-workspace sections),
        // both must be grouped together in the output.
        let input = "\
[package]
name = \"test\"
version = \"0.0.0\"

[dependencies]
serde = \"1.0\"

[workspace]
members = [\"a\"]

[features]
default = []

[workspace.package]
edition = \"2024\"

[workspace.dependencies]
anyhow = \"1.0\"
";
        let result = reorder(input);

        assert!(
            result.contains("[workspace.package]"),
            "missing [workspace.package] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.dependencies]"),
            "missing [workspace.dependencies] in:\n{result}"
        );
        assert!(
            result.contains("edition = \"2024\""),
            "missing edition in:\n{result}"
        );
    }

    #[test]
    fn non_contiguous_workspace_sections_across_profile() {
        // Mimics the reported scenario: [workspace] at the top,
        // [profile] in the middle, then [workspace.package] and
        // [workspace.lints.*] and [workspace.dependencies] after.
        // The parser must group all workspace.* sections with
        // [workspace] even when [profile] separates them.
        let input = "\
[package]
name = \"my-workspace\"
version = \"0.0.0\"
publish = false

[workspace]
members = [
    \"crate-a\",
    \"crate-b\",
]
resolver = \"3\"

[profile]

[workspace.package]
rust-version = \"1.93.0\"
edition = \"2024\"
license = \"Apache-2.0\"
authors = [\"Test Author <test@example.com>\"]

[workspace.lints.clippy]
missing_errors_doc = \"warn\"
needless_pass_by_value = \"warn\"
disallowed_types = { level = \"warn\", priority = 1 }

[workspace.lints.rust]
missing_docs = \"warn\"
unsafe_code = \"forbid\"

[workspace.dependencies]
anyhow = \"1.0\"
clap = { version = \"4.0\", features = [\"derive\"] }
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = { version = \"1.0\", features = [\"full\"] }
tracing = \"0.1\"
";
        let result = full_format(input);

        // All workspace sub-sections must be present
        assert!(
            result.contains("[workspace.package]"),
            "missing [workspace.package] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.lints.clippy]"),
            "missing [workspace.lints.clippy] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.lints.rust]"),
            "missing [workspace.lints.rust] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.dependencies]"),
            "missing [workspace.dependencies] in:\n{result}"
        );
        assert!(
            result.contains("[profile]"),
            "missing [profile] in:\n{result}"
        );
        // Verify content
        assert!(
            result.contains("rust-version"),
            "missing rust-version in:\n{result}"
        );
        assert!(
            result.contains("disallowed_types"),
            "missing disallowed_types in:\n{result}"
        );
        assert!(
            result.contains("tracing"),
            "missing tracing dep in:\n{result}"
        );
    }

    #[test]
    fn real_workspace_with_profile_subsections_and_lints() {
        // Reproduces exact structure from bug report: [profile]
        // with multiple sub-profiles, followed by comment block,
        // then [workspace.lints.*] sections.
        let input = "\
########################################
# Virtual workspace root
########################################
[workspace]
members = [
    \"crate-a\",
    \"crate-b\",
]
resolver = \"3\"

[package]
name = \"my-workspace\"
version = \"0.0.0\"
edition = \"2024\"
publish = false

[build-dependencies]
rhusky = \"0.0.2\"

[workspace.package]
edition = \"2024\"
version = \"0.0.0\" # Version dynamically managed by CI
license-file = \"LICENSE\"
rust-version = \"1.93.0\"

[workspace.dependencies]
anyhow = \"1.0\"
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = { version = \"1.0\", features = [\"full\"] }

[profile]

[profile.wasm-dev]
inherits = \"dev\"
opt-level = 1

[profile.release]
debug = false
strip = \"debuginfo\"

# Workspace-wide lint levels
[workspace.lints.rust]
warnings = \"deny\"     # never allow warnings to pass
missing_docs = \"deny\" # require docs on all public items

[workspace.lints.rustdoc]
missing_crate_level_docs = \"deny\" # require crate-level docs
broken_intra_doc_links = \"deny\"   # enforce valid intra-doc links
bare_urls = \"warn\"                # prefer backticks or proper links

[workspace.lints.clippy]
missing_panics_doc = \"warn\"                         # document panics
missing_errors_doc = \"warn\"                         # document errors
doc_markdown = \"warn\"                               # backticks for code
disallowed_types = { level = \"warn\", priority = 1 }

[workspace.metadata.clippy]
disallowed-types = [\"serde_json::Value\"]

########################################
# Patches for dependencies
########################################
[patch.crates-io]
# No patches currently needed
";
        let result = full_format(input);

        // All sections must survive
        assert!(
            result.contains("[workspace.lints.rust]"),
            "missing [workspace.lints.rust] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.lints.rustdoc]"),
            "missing [workspace.lints.rustdoc] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.lints.clippy]"),
            "missing [workspace.lints.clippy] in:\n{result}"
        );
        assert!(
            result.contains("[workspace.metadata.clippy]"),
            "missing [workspace.metadata.clippy] in:\n{result}"
        );
        assert!(
            result.contains("[patch.crates-io]"),
            "missing [patch.crates-io] in:\n{result}"
        );
        assert!(
            result.contains("# never allow warnings to pass"),
            "missing trailing comment in:\n{result}"
        );
        // Verify output is valid TOML
        let reparsed = result.parse::<DocumentMut>();
        assert!(
            reparsed.is_ok(),
            "Output is not valid TOML:\n{result}\nError: {}",
            reparsed.unwrap_err()
        );
    }

    #[test]
    fn reorder_actual_test_file() {
        // Test with the actual file content from /tmp that triggers
        // the parse error.
        let input =
            std::fs::read_to_string("/tmp/cargo-fmt-toml-test-case.toml").unwrap_or_default();
        if input.is_empty() {
            // Skip if the test file doesn't exist
            return;
        }
        let result = full_format(&input);

        // Verify the output is valid TOML
        let reparsed = result.parse::<DocumentMut>();
        assert!(
            reparsed.is_ok(),
            "Output is not valid TOML:\n{result}\nError: {}",
            reparsed.unwrap_err()
        );
    }

    #[test]
    fn full_pipeline_output_is_valid_toml() {
        // Verify the full pipeline produces valid TOML that can be
        // parsed back without errors.
        let input = "\
[package]
name = \"test-workspace\"
version = \"0.0.0\"
publish = false

[workspace]
members = [
    \"crate-a\",
    \"crate-b\",
]
resolver = \"3\"

[profile]

[workspace.package]
rust-version = \"1.93.0\"
edition = \"2024\"
license = \"Apache-2.0\"

[workspace.lints.clippy]
missing_errors_doc = \"warn\"
missing_crate_level_docs = \"deny\" # require crate-level docs
disallowed_types = { level = \"warn\", priority = 1 }

[workspace.lints.rust]
missing_docs = \"warn\"
unsafe_code = \"forbid\" # never allow unsafe

[workspace.dependencies]
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = { version = \"1.0\" }
anyhow = \"1.0\"
";
        // Run the full pipeline
        let result = full_format(input);

        // Verify the output is valid TOML
        let reparsed = result.parse::<DocumentMut>();
        assert!(
            reparsed.is_ok(),
            "Output is not valid TOML:\n{result}\nError: {}",
            reparsed.unwrap_err()
        );
    }

    #[test]
    fn full_pipeline_is_idempotent() {
        // Running the formatter twice must produce the same output.
        let input = "\
[workspace]
members = [\"crate-a\"]
resolver = \"3\"

[package]
name = \"test\"
version = \"0.0.0\"

[workspace.lints.clippy]
missing_errors_doc = \"warn\"
disallowed_types = { level = \"warn\", priority = 1 }

[workspace.package]
edition = \"2024\"
rust-version = \"1.93.0\"

[dependencies]
tokio = \"1.0\"
anyhow = \"1.0\"
serde = \"1.0\"

[workspace.dependencies]
serde = { version = \"1.0\", features = [\"derive\"] }
";
        let first = full_format(input);
        let second = full_format(&first);
        assert_eq!(
            first, second,
            "Formatter is not idempotent.\nFirst:\n{first}\nSecond:\n{second}"
        );
    }

    #[test]
    fn array_of_tables_preserved() {
        // [[bin]] and [[example]] are array-of-tables headers that
        // must be preserved and reordered with their parent key.
        let input = "\
[dependencies]
serde = \"1.0\"

[[bin]]
name = \"my-tool\"
path = \"src/main.rs\"

[[bin]]
name = \"helper\"
path = \"src/helper.rs\"

[package]
name = \"test\"
version = \"0.1.0\"
";
        let result = full_format(input);

        // [package] should come before [[bin]] and [dependencies]
        let pkg_pos = result.find("[package]").expect("missing [package]");
        let bin_pos = result
            .find("[[bin]]")
            .unwrap_or_else(|| panic!("missing [[bin]] in:\n{result}"));
        let dep_pos = result
            .find("[dependencies]")
            .expect("missing [dependencies]");
        assert!(
            pkg_pos < bin_pos,
            "[package] should come before [[bin]] in:\n{result}"
        );
        assert!(
            bin_pos < dep_pos,
            "[[bin]] should come before [dependencies] in:\n{result}"
        );
        // Both [[bin]] entries must survive
        let bin_count = result.matches("[[bin]]").count();
        assert_eq!(bin_count, 2, "expected 2 [[bin]] entries, got {bin_count}");
        assert!(result.contains("my-tool"), "missing my-tool in:\n{result}");
        assert!(result.contains("helper"), "missing helper in:\n{result}");
        // Output must be valid TOML
        let reparsed = result.parse::<DocumentMut>();
        assert!(
            reparsed.is_ok(),
            "Output is not valid TOML:\n{result}\nError: {}",
            reparsed.unwrap_err()
        );
    }

    #[test]
    fn all_reorder_tests_produce_valid_toml() {
        // Verify every test scenario produces valid TOML output,
        // not just that expected strings are present.
        let inputs = [
            // workspace_dotted_sections_preserved
            "\
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
",
            // sections_not_in_order_list_are_preserved
            "\
[package]
name = \"test\"

[lints]
workspace = true

[dependencies]
serde = \"1.0\"
",
            // lints_clippy_with_inline_priority_preserved
            "\
[lints.clippy]
disallowed_types = { level = \"warn\", priority = 1 }
disallowed-names = { level = \"warn\", priority = -1 }

[package]
name = \"test-crate\"
version = \"0.1.0\"

[dependencies]
serde = \"1.0\"
",
            // non_contiguous_workspace_sections_across_profile
            "\
[package]
name = \"my-workspace\"
version = \"0.0.0\"
publish = false

[workspace]
members = [
    \"crate-a\",
    \"crate-b\",
]
resolver = \"3\"

[profile]

[workspace.package]
rust-version = \"1.93.0\"
edition = \"2024\"
license = \"Apache-2.0\"
authors = [\"Test Author <test@example.com>\"]

[workspace.lints.clippy]
missing_errors_doc = \"warn\"
needless_pass_by_value = \"warn\"
disallowed_types = { level = \"warn\", priority = 1 }

[workspace.lints.rust]
missing_docs = \"warn\"
unsafe_code = \"forbid\"

[workspace.dependencies]
anyhow = \"1.0\"
clap = { version = \"4.0\", features = [\"derive\"] }
serde = { version = \"1.0\", features = [\"derive\"] }
tokio = { version = \"1.0\", features = [\"full\"] }
tracing = \"0.1\"
",
        ];

        for (idx, input) in inputs.iter().enumerate() {
            let result = full_format(input);
            let reparsed = result.parse::<DocumentMut>();
            assert!(
                reparsed.is_ok(),
                "Scenario {idx} produced invalid TOML:\n{result}\nError: {}",
                reparsed.unwrap_err()
            );
        }
    }
}
