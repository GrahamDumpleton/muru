//! The committed configuration reference matches the types it is generated
//! from. `MURU_UPDATE_REFERENCE=1 cargo test -p muru-config --test reference`
//! (`just reference`) rewrites the copies under docs/.

use std::path::{Path, PathBuf};

use muru_config::schema::{self, File};

fn docs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// Compare `generated` with the file at `path`, or write it there when
/// asked to update, returning a complaint when they differ.
fn check(path: &Path, generated: &str) -> Option<String> {
    if std::env::var_os("MURU_UPDATE_REFERENCE").is_some() {
        std::fs::create_dir_all(path.parent().expect("a parent directory"))
            .expect("the directory is made");
        std::fs::write(path, generated).expect("the file is written");
        return None;
    }

    match std::fs::read_to_string(path) {
        Ok(committed) if committed == generated => None,
        _ => Some(format!("{} is stale", path.display())),
    }
}

#[test]
fn the_committed_reference_is_current() {
    let mut stale = Vec::new();

    for file in File::ALL {
        let json = serde_json::to_string_pretty(&schema::json_schema(file))
            .expect("the schema serializes")
            + "\n";
        let schema_path = docs().join(format!("_extra/schema/{}.schema.json", file.name()));
        let page_path = docs().join(format!("reference/configuration/{}.md", file.name()));

        stale.extend(check(&schema_path, &json));
        stale.extend(check(&page_path, &schema::reference(file)));
    }

    assert!(
        stale.is_empty(),
        "{}; run `just reference` to regenerate",
        stale.join(", ")
    );
}

#[test]
fn descriptions_cite_no_design_section() {
    let mut found = Vec::new();

    for file in File::ALL {
        found.extend(schema::citations(file));
    }

    found.sort();
    found.dedup();
    assert!(
        found.is_empty(),
        "doc comments users read still cite the design: {found:#?}"
    );
}

#[test]
fn citations_are_taken_out_of_descriptions() {
    let clean = schema::clean_description;

    assert_eq!(
        clean("Scheduled capacity (section 7.5)."),
        "Scheduled capacity."
    );
    assert_eq!(
        clean("Serve reflection (section 6.13, default\nfalse)."),
        "Serve reflection (default false)."
    );
    assert_eq!(
        clean("Scaled (sections 7.2 and 7.4); static."),
        "Scaled; static."
    );
    assert_eq!(clean("Section 6.13: on a listener"), "On a listener");
    assert_eq!(clean("Kept (default 1)."), "Kept (default 1).");
    assert_eq!(clean("One.\n\nTwo\nlines."), "One.\n\nTwo lines.");
}
