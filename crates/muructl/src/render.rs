//! `muructl render` (design specification section 10.4): the release a
//! command line describes, written as the table that declares it in the
//! daemon's format, for `muructl apply` or a record kept beside the code.
//! Nothing is asked of a daemon, and nothing is changed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Args;
use muru_controller::render::{Origin, Rendered};

use crate::admin::ReleaseArgs;
use crate::print_config::{COMMAND_LINE, DEFAULT, table_name};

/// The release id a table is written under when none is named.
const FIRST_RELEASE: &str = "r1";

/// What `render` takes.
#[derive(Args)]
pub struct RenderArgs {
    /// The application name.
    app: String,

    /// The source directory, written into the table as given here
    /// (default: the current directory, written as its full path).
    source: Option<PathBuf>,

    #[command(flatten)]
    settings: ReleaseArgs,

    /// The release id the table is written under (default: r1).
    #[arg(long, value_name = "ID")]
    release: Option<String>,

    /// Write every setting, the defaults included, each with where it
    /// came from.
    #[arg(long)]
    full: bool,

    /// Write JSON rather than TOML.
    #[arg(long)]
    json: bool,
}

/// Write the release the command line describes to standard output.
pub fn render(args: RenderArgs) -> anyhow::Result<()> {
    args.settings.check()?;

    if !muru_controller::model::valid_name(&args.app) {
        anyhow::bail!(
            "{:?} is not a valid application name: lowercase letters, digits and hyphens, up to 63 characters",
            args.app
        );
    }

    let id = args
        .release
        .clone()
        .unwrap_or_else(|| FIRST_RELEASE.to_owned());
    let id = id.as_str();

    if !muru_controller::model::valid_name(id) {
        anyhow::bail!("{id:?} is not a valid release id");
    }

    // The table names the source as the command line does, so that a
    // relative path stays relative to wherever the file is kept. With
    // none given there is nothing to keep, and the directory is named
    // in full.

    let (read, written) = match &args.source {
        Some(source) => (source.clone(), source.display().to_string()),
        None => {
            let here = std::env::current_dir().context("finding the current directory")?;
            (here.clone(), here.display().to_string())
        }
    };

    let read = read
        .canonicalize()
        .with_context(|| format!("{} does not exist", read.display()))?;

    if let Some(note) = crate::passed_over_note(&read) {
        eprintln!("muru: {note}");
    }

    let python = args
        .settings
        .python
        .as_ref()
        .map(|python| python.display().to_string())
        .unwrap_or_default();
    let RenderArgs {
        app,
        settings,
        full,
        json,
        ..
    } = args;
    let named = args.release.is_some();
    let request = settings.request(&app, &read.display().to_string(), python);
    let rendered =
        muru_controller::render::render(&request, &written, full).map_err(anyhow::Error::msg)?;

    let document = muru_config::Document {
        app: BTreeMap::from([(
            app.clone(),
            muru_config::App {
                release: BTreeMap::from([(id.to_owned(), rendered.table.clone())]),
                ..muru_config::App::default()
            },
        )]),
        ..muru_config::Document::default()
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    let text = document.to_toml().context("writing the release")?;
    let project = rendered
        .file
        .as_deref()
        .map(|file| table_name(Some(file), ""));
    let mut out = header(&rendered, project.as_deref(), &written, full, named);

    if full {
        // The opening comment names the file in full; beside each
        // setting its short name is enough, there being one such file.

        let short = match rendered.file.as_deref().and_then(Path::file_name) {
            Some(name) if name == "pyproject.toml" => "[tool.muru]".to_owned(),
            Some(name) => name.to_string_lossy().into_owned(),
            None => "the source".to_owned(),
        };

        let prefix = format!("app.{app}.release.{id}");
        out.push_str(&annotate(&text, &prefix, &rendered.origins, &short));
        out.push_str(&trailer(&rendered));
    } else {
        out.push_str(&text);
    }

    print!("{out}");
    Ok(())
}

/// The comment the file opens with: what it is, how it is used and how
/// its settings were arrived at.
fn header(
    rendered: &Rendered,
    project: Option<&str>,
    source: &str,
    full: bool,
    named: bool,
) -> String {
    let name = muru_config::DAEMON_FILE;
    let mut out = format!(
        "# The release this command line describes, in the daemon's format.\n\
         # Save it as {name} and give it to the daemon with:\n\
         #   muructl apply -f {name}\n"
    );

    match project {
        Some(project) => {
            let _ = writeln!(
                out,
                "# Settings come from the flags, then the source's own settings, then the\n\
                 # defaults. The source's settings are {project}."
            );
        }
        None => out.push_str("# Settings come from the flags, then the defaults.\n"),
    }

    out.push_str(if full {
        "# Every setting is written, with where it came from.\n"
    } else {
        "# A setting left at its default is not written; --full writes those too.\n"
    });

    if !named {
        let _ = writeln!(
            out,
            "# The release is written as {FIRST_RELEASE}. An applied file names its own releases,\n\
             # where muructl deploy numbers them; name this one with --release."
        );
    }

    if Path::new(source).is_relative() {
        let key = if rendered.table.cwd.is_some() {
            "cwd"
        } else {
            "source"
        };
        let _ = writeln!(
            out,
            "# {key} is a relative path, which muructl apply takes from this file's directory."
        );
    }

    out.push('\n');
    out
}

/// What `--full` says of the settings it cannot write: those that are
/// off unless set, and the interpreter, which the node finds.
fn trailer(rendered: &Rendered) -> String {
    let mut out = String::new();
    let off: Vec<&str> = rendered
        .unset
        .iter()
        .copied()
        .filter(|key| *key != "python")
        .collect();

    if !off.is_empty() {
        let _ = writeln!(out, "\n# Not set, and off unless set: {}.", off.join(", "));
    }

    if rendered.unset.contains(&"python") {
        if off.is_empty() {
            out.push('\n');
        }

        out.push_str(
            "# python is not set: the daemon finds the interpreter when the file is applied,\n\
             # as a deploy does.\n",
        );
    }

    out
}

/// Where a setting came from, as the comment beside it says.
fn describe(origin: Origin, project: &str) -> String {
    match (origin.source, origin.flags) {
        (true, true) => format!("{project}, then {COMMAND_LINE}"),
        (false, true) => COMMAND_LINE.to_owned(),
        (true, false) => project.to_owned(),
        (false, false) => DEFAULT.to_owned(),
    }
}

/// A key as a table header or a line writes it, without its quotes.
fn bare(key: &str) -> &str {
    let key = key.trim();
    key.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            key.strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(key)
}

/// The origin recorded for a table: its own, or that of the table it
/// lies in, since a table taken whole from the source's settings is
/// recorded once under its name.
fn of_table(origins: &BTreeMap<String, Origin>, table: &str) -> Option<Origin> {
    let mut name = table;

    loop {
        if let Some(origin) = origins.get(name) {
            return Some(*origin);
        }

        name = name.rsplit_once('.')?.0;
    }
}

/// A line of the written table and the comment that goes with it.
struct Line<'a> {
    text: &'a str,
    comment: Option<String>,
    /// The comment goes on a line of its own above, for a value that
    /// continues on the lines below as a string does.
    above: bool,
    /// The line belongs to the block of settings whose comments line up.
    setting: bool,
}

/// The table as written, with a comment beside each setting saying
/// where it came from, and beside each table taken whole from the
/// source's settings. `prefix` is the release's table, which the keys
/// of `origins` are relative to.
fn annotate(text: &str, prefix: &str, origins: &BTreeMap<String, Origin>, project: &str) -> String {
    let mut lines: Vec<Line<'_>> = Vec::new();

    // The table a line's keys belong to, relative to the release's, or
    // none outside it.

    let mut table: Option<String> = None;

    // What a value left open on an earlier line ends with.

    let mut open: Option<&str> = None;

    for line in text.lines() {
        let plain = |text| Line {
            text,
            comment: None,
            above: false,
            setting: false,
        };

        if let Some(end) = open {
            if line.trim_end().ends_with(end) {
                open = None;
            }

            lines.push(plain(line));
            continue;
        }

        if let Some(header) = line.strip_prefix('[') {
            let name = header.trim_start_matches('[').trim_end_matches(']');
            let quoted: Vec<&str> = name.split('.').map(bare).collect();
            let name = quoted.join(".");

            table = if name == prefix {
                Some(String::new())
            } else {
                name.strip_prefix(prefix)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .map(str::to_owned)
            };

            let comment = table
                .as_deref()
                .filter(|table| !table.is_empty())
                .and_then(|table| of_table(origins, table))
                .map(|origin| describe(origin, project));
            lines.push(Line {
                comment,
                ..plain(line)
            });
            continue;
        }

        let Some((key, value)) = line.split_once(" = ") else {
            lines.push(plain(line));
            continue;
        };

        // A string or a list may run on to the lines below.

        let value = value.trim_end();
        let string = ["\"\"\"", "'''"]
            .into_iter()
            .find(|quote| value.starts_with(quote))
            .filter(|quote| value.len() < 6 || !value.ends_with(quote));

        if let Some(quote) = string {
            open = Some(quote);
        } else if value == "[" {
            open = Some("]");
        }

        let comment = table.as_deref().and_then(|table| {
            let key = bare(key);

            let name = if table.is_empty() {
                key.to_owned()
            } else {
                format!("{table}.{key}")
            };

            origins.get(&name).map(|origin| describe(*origin, project))
        });
        lines.push(Line {
            comment,
            above: string.is_some(),
            setting: true,
            ..plain(line)
        });
    }

    // The comments of a table's settings line up, unless one long value
    // would push them all off the screen.

    let mut out = String::new();
    let mut start = 0;

    while start < lines.len() {
        // A block runs from one table's header to the next.

        let end = lines[start + 1..]
            .iter()
            .position(|line| !line.setting && line.text.starts_with('['))
            .map_or(lines.len(), |offset| start + 1 + offset);
        let block = &lines[start..end];
        let width = block
            .iter()
            .filter(|line| line.setting && line.comment.is_some() && !line.above)
            .map(|line| line.text.len())
            .filter(|length| *length <= 44)
            .max()
            .unwrap_or(0);

        for line in block {
            match &line.comment {
                Some(comment) if line.above => {
                    let _ = writeln!(out, "# {comment}\n{}", line.text);
                }
                Some(comment) if line.setting => {
                    let _ = writeln!(out, "{:<width$}    # {comment}", line.text);
                }
                Some(comment) => {
                    let _ = writeln!(out, "{}    # {comment}", line.text);
                }
                None => {
                    let _ = writeln!(out, "{}", line.text);
                }
            }
        }

        start += block.len();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITTEN: &str = r#"[app.shop.release.r1]
source = "."
entrypoint = "shop.wsgi:application"
processes = 4
threads = 6
sendfile_roots = [
    "/srv/media",
    "/srv/other",
]
hooks = """
one
two = 2
"""

[app.shop.release.r1.build]
when = "auto"
extras = ["postgres", "redis"]

[app.shop.release.r1.groups.reports]
paths = ["/reports/"]

[app.shop.release.r1.groups.reports.timeouts]
soft_timeout = "9s"

[[app.shop.release.r1.static]]
path = "/static/"
dir = "assets"

[app.shop.release.r1.env]
A_VERY_LONG_NAME_OF_A_VARIABLE = "with a value that is long as well"
MODE = "production"

[app.shop.release.r1.env.TOKEN.secret]
store = "vault"
key = "shop/token"

[app.shop.release.r1.error_pages."404"]
file = "errors/404.html"
"#;

    fn origins() -> BTreeMap<String, Origin> {
        let both = Origin {
            source: true,
            flags: true,
        };

        [
            ("entrypoint", Origin::SOURCE),
            ("processes", Origin::FLAGS),
            ("threads", Origin::DEFAULT),
            ("sendfile_roots", Origin::SOURCE),
            ("hooks", Origin::FLAGS),
            ("build.when", Origin::DEFAULT),
            ("build.extras", both),
            ("groups", Origin::SOURCE),
            ("static", both),
            ("env.A_VERY_LONG_NAME_OF_A_VARIABLE", Origin::SOURCE),
            ("env.MODE", Origin::SOURCE),
            ("env.TOKEN", Origin::FLAGS),
            ("error_pages", Origin::SOURCE),
        ]
        .into_iter()
        .map(|(key, origin)| (key.to_owned(), origin))
        .collect()
    }

    #[test]
    fn each_setting_is_followed_by_where_it_came_from() {
        let annotated = annotate(WRITTEN, "app.shop.release.r1", &origins(), "FILE");

        for line in [
            "[app.shop.release.r1]",
            "source = \".\"",
            "entrypoint = \"shop.wsgi:application\"    # FILE",
            "processes = 4                           # command line",
            "threads = 6                             # default",
            "sendfile_roots = [                      # FILE",
            "    \"/srv/media\",",
            "# command line\nhooks = \"\"\"\none\ntwo = 2\n\"\"\"",
            "when = \"auto\"                     # default",
            "extras = [\"postgres\", \"redis\"]    # FILE, then command line",
            "[app.shop.release.r1.groups.reports]    # FILE",
            "paths = [\"/reports/\"]",
            "[app.shop.release.r1.groups.reports.timeouts]    # FILE",
            "soft_timeout = \"9s\"",
            "[[app.shop.release.r1.static]]    # FILE, then command line",
            "A_VERY_LONG_NAME_OF_A_VARIABLE = \"with a value that is long as well\"    # FILE",
            "MODE = \"production\"    # FILE",
            "[app.shop.release.r1.env]",
            "[app.shop.release.r1.env.TOKEN.secret]    # command line",
            "[app.shop.release.r1.error_pages.\"404\"]    # FILE",
        ] {
            assert!(
                annotated.lines().any(|written| written == line)
                    || (line.contains('\n') && annotated.contains(line)),
                "{line:?} is not in:\n{annotated}"
            );
        }
    }

    #[test]
    fn the_comments_leave_the_table_as_it_was() {
        let annotated = annotate(WRITTEN, "app.shop.release.r1", &origins(), "FILE");
        let before: toml::Value = toml::from_str(WRITTEN).unwrap();
        let after: toml::Value = toml::from_str(&annotated).unwrap();
        assert_eq!(before, after);

        // A table of another application is left alone.

        let other = "[app.shopping.release.r1]\nprocesses = 4\n";
        assert_eq!(
            annotate(other, "app.shop.release.r1", &origins(), "FILE"),
            other
        );
    }
}
