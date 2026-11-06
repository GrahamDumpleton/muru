//! The JSON Schema of each configuration file, generated from the types
//! that parse it, and the configuration reference pages rendered from those
//! schemas (design specification section 18.3).
//!
//! The descriptions are the types' doc comments, with their citations of
//! the design specification and its decision records taken out, since the
//! schema and the pages are read by users. `tests/reference.rs` compares
//! what this generates with the copies committed under docs/, and rewrites
//! them when `MURU_UPDATE_REFERENCE` is set.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use schemars::JsonSchema;
use serde_json::{Map, Value};

use crate::{BuildWhen, Document, RunSettings, ToolMuru};

/// The keyword a field's default is described by when it has no single
/// value: `#[schemars(extend("x-default" = "..."))]`.
const DEFAULT_TEXT: &str = "x-default";

/// Where the published schemas live, for their `$id`.
const SCHEMA_BASE: &str = "https://docs.muruhq.dev/en/latest/schema";

/// A configuration file with a schema and a reference page of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum File {
    /// The daemon's configuration file and the files `muructl apply` reads.
    Daemon,
    /// The `[tool.muru]` table of a pyproject.toml, or a muru.toml.
    Project,
    /// muru.local.toml, a developer's own settings for `muructl run`.
    Local,
}

impl File {
    /// Every file, in the order the reference lists them.
    pub const ALL: [File; 3] = [File::Daemon, File::Project, File::Local];

    /// The name of the file's schema and page: `daemon` gives
    /// daemon.schema.json and daemon.md.
    pub fn name(self) -> &'static str {
        match self {
            File::Daemon => "daemon",
            File::Project => "project",
            File::Local => "local",
        }
    }

    fn title(self) -> &'static str {
        match self {
            File::Daemon => "The daemon's configuration and applied files",
            File::Project => "The project's [tool.muru] table",
            File::Local => "muru.local.toml",
        }
    }

    /// The table the file's keys sit in, as its headings name it.
    fn root(self) -> &'static str {
        match self {
            File::Project => "tool.muru",
            File::Daemon | File::Local => "",
        }
    }

    fn introduction(self) -> &'static str {
        match self {
            File::Daemon => {
                "The keys of the daemon's configuration file, `muru.toml` in its \
                 configuration directory, and of the files `muructl apply` reads, which \
                 share one format. A file in this format kept beside a project's code is \
                 named `muru.daemon.toml`, since `muru.toml` there is the project's \
                 settings, which have other keys. Listeners, limits, the proxy, compression, secret \
                 stores, logs, shutdown, metrics, events, the dashboard, the overlay \
                 webhook, client classes and rate limits belong to the daemon's own file; \
                 an applied file that sets them is refused. Applications, their releases, \
                 hosts, routes, upstreams and overlays may be in either."
            }
            File::Project => {
                "The keys an application ships with: the `[tool.muru]` table of its \
                 `pyproject.toml`, or the same keys at the top level of a `muru.toml` \
                 beside it. A project with both uses its `muru.toml`: the `[tool.muru]` \
                 table is then not read, and the two are never merged. `muructl serve \
                 --config FILE` reads FILE, in the form of a `muru.toml`, in place of \
                 either. Every key is optional. A command line flag, or a release \
                 declared to the daemon, takes precedence over the key."
            }
            File::Local => {
                "A developer's own settings for `muructl run`, in `muru.local.toml` beside \
                 the project, which is not committed. Its `[run]` table holds the keys of \
                 `[tool.muru.run]` and takes precedence over them; lists add to the \
                 project's."
            }
        }
    }
}

/// muru.local.toml as a schema sees it: the `[run]` table, and the one
/// key of `[build]` development reads. The file is parsed by
/// [`crate::parse_local`], which takes the tables apart itself.
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "only its schema is used")]
struct LocalFile {
    /// How `muructl run` serves the project, over `[tool.muru.run]`.
    run: Option<RunSettings>,
    /// When `muructl run` builds the environment.
    build: Option<LocalBuild>,
}

/// The `[build]` table of muru.local.toml.
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[expect(dead_code, reason = "only its schema is used")]
struct LocalBuild {
    /// When the environment is built before the application starts.
    when: Option<BuildWhen>,
}

/// The file's JSON Schema, with descriptions for users.
pub fn json_schema(file: File) -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();

    let schema = match file {
        File::Daemon => generator.into_root_schema_for::<Document>(),
        File::Project => generator.into_root_schema_for::<ToolMuru>(),
        File::Local => generator.into_root_schema_for::<LocalFile>(),
    };

    let mut value = schema.to_value();
    drop_null(&mut value);
    clean_descriptions(&mut value);

    if let Value::Object(object) = &mut value {
        let mut ordered = Map::new();
        ordered.insert(
            "$schema".into(),
            object.remove("$schema").unwrap_or_default(),
        );
        ordered.insert(
            "$id".into(),
            Value::String(format!("{SCHEMA_BASE}/{}.schema.json", file.name())),
        );
        ordered.insert("title".into(), Value::String(file.title().into()));
        object.remove("title");
        ordered.extend(std::mem::take(object));
        *object = ordered;
    }

    value
}

/// The keys the file takes at its top level.
pub fn top_level_keys(file: File) -> BTreeSet<String> {
    json_schema(file)
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

/// Citations of the design specification left in the file's descriptions
/// after cleaning, which a doc comment must be reworded to drop.
pub fn citations(file: File) -> Vec<String> {
    let mut found = Vec::new();
    collect_citations(&json_schema(file), &mut found);
    found
}

fn collect_citations(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "description" {
                    if let Value::String(text) = value
                        && CITATION.is_match(text)
                    {
                        found.push(text.clone());
                    }
                } else {
                    collect_citations(value, found);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_citations(item, found);
            }
        }
        _ => {}
    }
}

static CITATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:sections? \d|appendix [a-z]\b|appendices|ADRs? \d|design specification)")
        .expect("a valid pattern")
});

/// One part of a parenthesis that only cites: "section 6.13", "sections
/// 7.2 and 7.4", "ADR 0033", "appendix C".
static CITING_PART: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:see )?(?:the )?(?:design specification,? )?(?:sections? [\d.]+(?:(?:,| and| or) [\d.]+)*|appendix [a-z]|ADRs? \d+(?:(?:,| and) \d+)*)$",
    )
    .expect("a valid pattern")
});

static PARENTHESIS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" ?\(([^()]*)\)").expect("a valid pattern"));

static LEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Section [\d.]+: (.)").expect("a valid pattern"));

/// Take out the null an optional key's schema allows: TOML has no null,
/// and leaving the key out is how it is not given.
fn drop_null(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(kinds)) = object.get_mut("type") {
                kinds.retain(|kind| kind != "null");

                if kinds.len() == 1 {
                    let kind = kinds.remove(0);
                    object.insert("type".into(), kind);
                }
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(variants)) = object.get_mut(key) {
                    variants.retain(|variant| variant.get("type") != Some(&Value::from("null")));

                    if variants.len() == 1 {
                        let only = variants.remove(0);
                        object.remove(key);

                        if let Value::Object(only) = only {
                            for (name, value) in only {
                                object.entry(name).or_insert(value);
                            }
                        }
                    }
                }
            }

            object.values_mut().for_each(drop_null);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_null),
        _ => {}
    }
}

fn clean_descriptions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if key == "description" {
                    if let Value::String(text) = value {
                        *text = clean_description(text);
                    }
                } else {
                    clean_descriptions(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(clean_descriptions),
        _ => {}
    }
}

/// A doc comment as a user reads it: paragraphs on one line each, and the
/// citations taken out.
pub fn clean_description(text: &str) -> String {
    let joined = text
        .split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n\n");

    let uncited = PARENTHESIS.replace_all(&joined, |captures: &regex::Captures<'_>| {
        let kept: Vec<&str> = captures[1]
            .split([',', ';'])
            .map(str::trim)
            .filter(|part| !CITING_PART.is_match(part))
            .collect();

        if kept.len() == captures[1].split([',', ';']).count() {
            captures[0].to_string()
        } else if kept.is_empty() {
            String::new()
        } else {
            format!(" ({})", kept.join(", "))
        }
    });

    LEADING
        .replace(&uncited, |captures: &regex::Captures<'_>| {
            captures[1].to_uppercase()
        })
        .into_owned()
}

/// The file's reference page, in MyST Markdown.
pub fn reference(file: File) -> String {
    let schema = json_schema(file);
    let definitions = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut page = Page {
        file,
        definitions: &definitions,
        placed: BTreeMap::new(),
        described: BTreeSet::new(),
        used: BTreeMap::new(),
        text: String::new(),
    };

    page.place(&schema);
    page.table(file.root(), &schema, None);

    let mut text = format!(
        "<!-- Generated from crates/muru-config by tests/reference.rs; do not edit. -->\n\n\
         # {}\n\n{}\n\n\
         The JSON Schema of the same keys, for an editor that checks TOML against one, is \
         `{SCHEMA_BASE}/{}.schema.json`.\n\n",
        file.title(),
        file.introduction(),
        file.name(),
    );
    text.push_str(page.text.trim_end());
    text.push('\n');
    text
}

/// A table below a key: where it sits, and the definition it is.
struct Child {
    path: String,
    definition: String,
}

/// A reference page being built. A table used in several places is
/// described once, under the shortest path it has, and linked from the
/// others.
struct Page<'a> {
    file: File,
    definitions: &'a Map<String, Value>,
    /// The path each definition is described under.
    placed: BTreeMap<String, String>,
    /// The definitions described so far.
    described: BTreeSet<String>,
    /// The paths each definition is used at.
    used: BTreeMap<String, Vec<String>>,
    text: String,
}

impl Page<'_> {
    /// Decide where each table is described: breadth first, so the
    /// shortest path to it wins.
    fn place(&mut self, root: &Value) {
        let mut queue = VecDeque::from([(self.file.root().to_string(), root)]);

        while let Some((path, schema)) = queue.pop_front() {
            for child in self.children(&path, schema) {
                let used = self.used.entry(child.definition.clone()).or_default();

                if !used.contains(&child.path) {
                    used.push(child.path.clone());
                }

                if self.placed.contains_key(&child.definition) {
                    continue;
                }

                self.placed
                    .insert(child.definition.clone(), child.path.clone());

                if let Some(target) = self.definitions.get(&child.definition) {
                    queue.push_back((child.path, target));
                }
            }
        }
    }

    fn children(&self, path: &str, schema: &Value) -> Vec<Child> {
        let mut children = Vec::new();

        for (key, property) in properties(resolve(self.definitions, schema)) {
            self.kind(&join(path, key), property, &mut children);
        }

        children
    }

    /// Describe the table at `path`, then the tables placed below it.
    fn table(&mut self, path: &str, schema: &Value, definition: Option<&str>) {
        let definitions = self.definitions;
        let schema = resolve(definitions, schema);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let title = if path.is_empty() {
            "Top level".to_string()
        } else {
            heading(path)
        };
        let _ = writeln!(self.text, "({})=\n## {title}\n", self.label(path));

        if let Some(definition) = definition {
            if let Some(description) = schema.get("description").and_then(Value::as_str) {
                let _ = writeln!(self.text, "{}\n", description.replace("\n\n", " "));
            }

            let others: Vec<String> = self
                .used
                .get(definition)
                .into_iter()
                .flatten()
                .filter(|other| *other != path)
                .map(|other| heading(other))
                .collect();

            if !others.is_empty() {
                let _ = writeln!(
                    self.text,
                    "The same keys apply at {}.\n",
                    list(&others, "and")
                );
            }
        }

        let _ = writeln!(
            self.text,
            "| Key | Type | Default | Description |\n| --- | --- | --- | --- |"
        );
        let mut children = Vec::new();

        for (key, property) in properties(schema) {
            let kind = self.kind(&join(path, key), property, &mut children);
            let mut description = property
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .replace("\n\n", " ");

            if let Some(values) = self.values(property) {
                description = format!("{description} {values}");
            }

            let _ = writeln!(
                self.text,
                "| `{key}` | {kind} | {} | {} |",
                if required.contains(&key.as_str()) {
                    "required".to_string()
                } else {
                    default(property).replace('|', "\\|")
                },
                description.trim().replace('|', "\\|")
            );
        }

        self.text.push('\n');

        for child in children {
            if self.placed.get(&child.definition) != Some(&child.path)
                || !self.described.insert(child.definition.clone())
            {
                continue;
            }

            if let Some(target) = definitions.get(&child.definition) {
                self.table(&child.path, target, Some(&child.definition));
            }
        }
    }

    /// The type of the key at `path`, as the table shows it, noting any
    /// table below it in `children`.
    fn kind(&self, path: &str, property: &Value, children: &mut Vec<Child>) -> String {
        if let Some(definition) = reference_name(property) {
            let Some(target) = self.definitions.get(definition) else {
                return "any".to_string();
            };

            if target.get("properties").is_none() {
                return self.kind(path, target, children);
            }

            children.push(Child {
                path: path.to_string(),
                definition: definition.to_string(),
            });
            return format!("table, {}", self.link(definition, path));
        }

        match property.get("type").and_then(Value::as_str) {
            Some("array") => {
                let items = property.get("items").unwrap_or(&Value::Null);
                let is_table = reference_name(items)
                    .and_then(|definition| self.definitions.get(definition))
                    .is_some_and(|target| target.get("properties").is_some());

                if is_table {
                    let item = self.kind(&format!("[{path}]"), items, children);
                    format!("array of {}", plural(&item))
                } else {
                    format!("array of {}", plural(&self.kind(path, items, children)))
                }
            }
            Some("object") => match property.get("additionalProperties") {
                Some(values) if values.is_object() => {
                    let key = path.rsplit('.').next().unwrap_or(path);
                    let placeholder = placeholder(key);
                    let value = self.kind(&format!("{path}.{placeholder}"), values, children);
                    format!("table of {}", plural(&value))
                }
                _ => "table".to_string(),
            },
            Some(kind) => kind.to_string(),
            None => {
                let variants = property
                    .get("oneOf")
                    .or_else(|| property.get("anyOf"))
                    .and_then(Value::as_array);
                let Some(variants) = variants else {
                    return "any".to_string();
                };

                let mut kinds: Vec<String> = Vec::new();

                for variant in variants {
                    let kind = if variant.get("const").is_some() || variant.get("enum").is_some() {
                        "string".to_string()
                    } else {
                        self.kind(path, variant, children)
                    };

                    if !kinds.contains(&kind) {
                        kinds.push(kind);
                    }
                }

                kinds.join(" or ")
            }
        }
    }

    /// The values a key takes when it is one of a set, as a sentence, with
    /// what each means when the type says.
    fn values(&self, property: &Value) -> Option<String> {
        let property = resolve(self.definitions, property);

        if property.get("type").and_then(Value::as_str) == Some("array") {
            return self.values(property.get("items")?);
        }

        if let Some(values) = property.get("enum").and_then(Value::as_array) {
            let names: Vec<String> = values
                .iter()
                .filter_map(Value::as_str)
                .map(|value| format!("`{value}`"))
                .collect();
            return Some(format!("One of {}.", list(&names, "or")));
        }

        let variants = property.get("oneOf").and_then(Value::as_array)?;
        let mut described = Vec::new();
        let mut names = Vec::new();

        for variant in variants {
            let values: Vec<&str> = match variant.get("const") {
                Some(Value::String(value)) => vec![value.as_str()],
                _ => variant
                    .get("enum")
                    .and_then(Value::as_array)
                    .map(|values| values.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default(),
            };

            if values.is_empty() {
                return None;
            }

            let quoted: Vec<String> = values.iter().map(|value| format!("`{value}`")).collect();
            names.extend(quoted.iter().cloned());

            if let Some(description) = variant.get("description").and_then(Value::as_str) {
                let description = description.replace("\n\n", " ");
                described.push(format!(
                    "{}: {}",
                    list(&quoted, "or"),
                    sentence(&description)
                ));
            }
        }

        if described.len() == variants.len() {
            Some(described.join(" "))
        } else {
            Some(format!("One of {}.", list(&names, "or")))
        }
    }

    /// A link to the section describing a definition.
    fn link(&self, definition: &str, path: &str) -> String {
        let path = self.placed.get(definition).map_or(path, String::as_str);
        format!(
            "{{ref}}`{} <{}>`",
            heading(path).replace('`', ""),
            self.label(path)
        )
    }

    fn label(&self, path: &str) -> String {
        let mut label = format!("config-{}", self.file.name());

        for part in path.split(|c: char| !c.is_ascii_alphanumeric()) {
            if !part.is_empty() {
                label.push('-');
                label.push_str(&part.to_ascii_lowercase());
            }
        }

        label
    }
}

/// A key's default as the table shows it: the value as TOML writes it, or
/// a phrase when it depends on where the setting is used.
fn default(property: &Value) -> String {
    let empty = |value: &Value| match value {
        Value::Array(items) => items.is_empty(),
        Value::Object(entries) => entries.is_empty(),
        _ => false,
    };

    if let Some(value) = property.get("default").filter(|value| !empty(value)) {
        return format!("`{}`", toml_value(value));
    }

    property
        .get(DEFAULT_TEXT)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn toml_value(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(toml_value).collect();
            format!("[{}]", items.join(", "))
        }
        other => other.to_string(),
    }
}

fn properties(schema: &Value) -> impl Iterator<Item = (&String, &Value)> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
}

fn resolve<'a>(definitions: &'a Map<String, Value>, schema: &'a Value) -> &'a Value {
    match reference_name(schema) {
        Some(definition) => definitions.get(definition).unwrap_or(schema),
        None => schema,
    }
}

fn reference_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

/// The path of `key` in the table at `path`. A key of an array of tables
/// belongs to its last entry, which TOML names by the array's path.
fn join(path: &str, key: &str) -> String {
    let path = path.trim_start_matches('[').trim_end_matches(']');

    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// A table's heading: `[path]`, or `[[path]]` for an array of tables,
/// whose path carries its own brackets.
fn heading(path: &str) -> String {
    format!("`[{path}]`")
}

/// What the keys of a table of tables stand for in its path.
fn placeholder(key: &str) -> &'static str {
    match key {
        "release" => "ID",
        "error_pages" => "STATUS",
        _ => "NAME",
    }
}

/// A type as the type of several things: "strings", "tables, ...".
fn plural(kind: &str) -> String {
    kind.split(" or ")
        .map(|kind| match kind.split_once(", ") {
            Some((word, rest)) => format!("{}, {rest}", plural_word(word)),
            None => plural_word(kind),
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

fn plural_word(word: &str) -> String {
    match word {
        "any" => "any values".to_string(),
        word if word.starts_with("array of ") => format!("arrays of {}", &word[9..]),
        word => format!("{word}s"),
    }
}

/// Items as a sentence lists them: "a, b or c".
fn list(items: &[String], conjunction: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} {conjunction} {last}", rest.join(", ")),
    }
}

fn sentence(text: &str) -> String {
    let text = text.trim();

    if text.ends_with('.') {
        text.to_string()
    } else {
        format!("{text}.")
    }
}
