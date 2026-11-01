//! Embeds the user guide's Markdown pages for the MCP server's docs tool,
//! as the dashboard embeds its assets: a table of each page's name and
//! text, generated from what docs/ holds at build time. A build from a
//! source tree without docs/ gets an empty table.

use std::path::{Path, PathBuf};

/// The parts of the user guide an agent reads; the project's own pages
/// (the specification and decision records) are left out.
const SECTIONS: &[&str] = &["tutorials", "howto", "explanation", "reference"];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let docs = manifest.join("../../docs");
    let mut pages: Vec<(String, PathBuf)> = Vec::new();

    for section in SECTIONS {
        let directory = docs.join(section);
        println!("cargo:rerun-if-changed={}", directory.display());
        collect(&directory, section, &mut pages);
    }

    pages.sort();

    let mut table =
        String::from("/// The user guide's pages, by name, as the docs tool returns them.\n");
    table.push_str("pub static PAGES: &[(&str, &str)] = &[\n");

    for (name, path) in &pages {
        table.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }

    table.push_str("];\n");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    std::fs::write(out.join("guide.rs"), table).expect("writing the guide table");
}

/// The Markdown pages of one section, named section/page without the
/// extension; index pages are kept, as each section's contents.
fn collect(directory: &Path, section: &str, pages: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.extension().is_some_and(|extension| extension == "md") {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_owned();
            let path = path.canonicalize().unwrap_or(path);
            pages.push((format!("{section}/{stem}"), path));
        }
    }
}
