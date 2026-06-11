//! Structural checks for the reference registry at `docs/references/`.
//!
//! The registry convention (one file per source, frontmatter schema, INDEX
//! coverage, self containment) is documented in `docs/references/README.md`.
//! These tests hold the structure honest without touching the network: keys
//! present, slugs consistent, INDEX complete, referenced paths real. Hash
//! and live URL verification is deliberately out of scope for `cargo test`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const COMPANIONS: [&str; 4] = ["README.md", "INDEX.md", "GLOSSARY.md", "VERIFICATION-MAP.md"];

const REQUIRED_KEYS: [&str; 15] = [
    "slug",
    "category",
    "citation",
    "canonical",
    "doi",
    "archived",
    "archive_date",
    "retrieved",
    "sha256",
    "license",
    "vendor_status",
    "rot_risk",
    "consumers",
    "provenance",
    "verification",
];

const ALLOWED_CATEGORIES: [&str; 7] =
    ["spec", "conformance", "oracle", "algorithms", "registries", "history", "failure-museum"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn registry_dir() -> PathBuf {
    repo_root().join("docs/references")
}

/// Entry files: every `*.md` directly in the registry except the companions.
fn entry_files() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(registry_dir())
        .expect("docs/references/ must exist")
        .map(|e| e.expect("readable dir entry").path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "md")
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !COMPANIONS.contains(&n))
        })
        .collect();
    out.sort();
    assert!(
        !out.is_empty(),
        "the registry has no entries; the bootstrap promised at least the spec, oracle, and corpus"
    );
    out
}

/// The frontmatter block: the lines between the first `---` line and the next.
fn frontmatter(path: &Path) -> Vec<String> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("---"),
        "{}: an entry must open with a `---` frontmatter fence",
        path.display()
    );
    let block: Vec<String> = lines.take_while(|l| *l != "---").map(str::to_string).collect();
    assert!(
        text.lines().skip(1).any(|l| l == "---"),
        "{}: the frontmatter fence never closes",
        path.display()
    );
    block
}

/// The value of a top level `key:` line inside a frontmatter block.
fn value_of(block: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    block
        .iter()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].trim().trim_matches('"').to_string())
}

/// List items (`  - path`) following `key:` until the next top level key.
fn list_of(block: &[String], key: &str) -> Vec<String> {
    let prefix = format!("{key}:");
    let mut out = Vec::new();
    let mut in_list = false;
    for line in block {
        if line.starts_with(&prefix) {
            in_list = true;
            continue;
        }
        if in_list {
            if let Some(item) = line.strip_prefix("  - ") {
                out.push(item.trim().to_string());
            } else {
                break;
            }
        }
    }
    out
}

#[test]
fn every_entry_has_the_full_schema() {
    for path in entry_files() {
        let block = frontmatter(&path);
        for key in REQUIRED_KEYS {
            assert!(
                value_of(&block, key).is_some() || !list_of(&block, key).is_empty(),
                "{}: missing frontmatter key `{key}`",
                path.display()
            );
        }
    }
}

#[test]
fn slugs_match_filenames_and_categories_are_known() {
    for path in entry_files() {
        let block = frontmatter(&path);
        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert_eq!(
            value_of(&block, "slug").as_deref(),
            Some(stem),
            "{}: slug must equal the filename stem",
            path.display()
        );
        let category = value_of(&block, "category").unwrap_or_default();
        for token in category.split(',').map(str::trim) {
            assert!(
                ALLOWED_CATEGORIES.contains(&token),
                "{}: category `{token}` is not in the allowed set {ALLOWED_CATEGORIES:?}",
                path.display()
            );
        }
    }
}

#[test]
fn archives_are_resolved_not_placeholders() {
    for path in entry_files() {
        let block = frontmatter(&path);
        let archived = value_of(&block, "archived").unwrap_or_default();
        assert!(
            archived != "ARCHIVE_PENDING",
            "{}: the Wayback save was never recorded; archive at citation time or record the documented fallback",
            path.display()
        );
    }
}

#[test]
fn index_covers_exactly_the_entries() {
    let index = fs::read_to_string(registry_dir().join("INDEX.md")).expect("INDEX.md must exist");
    let entries: BTreeSet<String> = entry_files()
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    for name in &entries {
        let link = format!("({name})");
        assert_eq!(index.matches(&link).count(), 1, "INDEX.md must link `{name}` exactly once");
    }
    // Every markdown link target in INDEX.md must exist in the registry.
    for target in index.split("](").skip(1) {
        let Some(target) = target.split(')').next() else {
            continue;
        };
        let is_md = Path::new(target).extension().is_some_and(|x| x.eq_ignore_ascii_case("md"));
        if is_md && !target.contains('/') {
            assert!(
                registry_dir().join(target).exists(),
                "INDEX.md links `{target}` which does not exist"
            );
        }
    }
}

#[test]
fn consumers_and_vendored_paths_exist() {
    let root = repo_root();
    for path in entry_files() {
        let block = frontmatter(&path);
        for consumer in list_of(&block, "consumers") {
            assert!(
                root.join(&consumer).exists(),
                "{}: consumer path `{consumer}` does not exist",
                path.display()
            );
        }
        let vendor = value_of(&block, "vendor_status").unwrap_or_default();
        if let Some(vendored) = vendor.strip_prefix("vendored-at-path ") {
            assert!(
                root.join(vendored.trim()).exists(),
                "{}: vendored path `{vendored}` does not exist",
                path.display()
            );
        }
    }
}

#[test]
fn corpus_entry_accounts_for_every_committed_vector() {
    let entry = fs::read_to_string(registry_dir().join("conformance-vector-corpus.md"))
        .expect("the corpus entry must exist");
    let vectors_dir = repo_root().join("tests/vectors");
    let mut count = 0usize;
    for file in fs::read_dir(&vectors_dir).expect("tests/vectors/ must exist") {
        let path = file.expect("readable dir entry").path();
        if path.extension().is_some_and(|x| x == "bin") {
            count += 1;
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                entry.contains(name),
                "corpus entry does not mention committed vector `{name}`; \
                 update conformance-vector-corpus.md in the same slice that adds a vector"
            );
        }
    }
    assert!(count > 0, "tests/vectors/ holds no images");
}
