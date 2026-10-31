//! The part of PEP 440 that `requires-python` and a uv version
//! constraint need: release versions such as `3.13` or `3.14.0`, and
//! specifier sets such as `>=3.12,<4` or `==3.13.*`. Pre-release and
//! local segments are read past and otherwise ignored, since the
//! interpreters compared here are release builds.

use std::cmp::Ordering;
use std::fmt;

/// A release version: its numeric segments, compared with the shorter one
/// padded with zeros, so `3.13` and `3.13.0` are equal.
#[derive(Debug, Clone)]
pub struct Version {
    segments: Vec<u64>,
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl std::hash::Hash for Version {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.significant().hash(state);
    }
}

impl Version {
    /// Parse a version. Anything after the release segments (`rc1`, `+local`,
    /// a free-threading `t`) is accepted and dropped.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let mut segments = Vec::new();
        let mut rest = text;

        loop {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();

            if digits.is_empty() {
                return None;
            }

            segments.push(digits.parse().ok()?);
            rest = &rest[digits.len()..];

            match rest.strip_prefix('.') {
                Some(after) if after.starts_with(|c: char| c.is_ascii_digit()) => rest = after,
                _ => break,
            }
        }

        // The rest must look like a pre-release, post, dev or local
        // segment, or the free-threading suffix, not a second number.

        let valid_tail = rest.is_empty()
            || rest.starts_with(['a', 'b', 'c', 'r', 't', '-', '_', '+'])
            || [".post", ".dev", ".a", ".b", ".c", ".rc"]
                .iter()
                .any(|suffix| rest.starts_with(suffix));

        valid_tail.then_some(Self { segments })
    }

    /// The major version, the first segment.
    pub fn major(&self) -> u64 {
        self.segments.first().copied().unwrap_or(0)
    }

    /// The minor version, the second segment, zero when absent.
    pub fn minor(&self) -> u64 {
        self.segments.get(1).copied().unwrap_or(0)
    }

    /// Whether this version starts with every segment of `prefix`, which is
    /// what `==3.13.*` asks.
    fn starts_with(&self, prefix: &Version) -> bool {
        prefix
            .segments
            .iter()
            .enumerate()
            .all(|(index, segment)| self.segments.get(index).copied().unwrap_or(0) == *segment)
    }

    /// The segments with trailing zeros dropped, at least one kept.
    fn significant(&self) -> &[u64] {
        let mut end = self.segments.len();

        while end > 1 && self.segments[end - 1] == 0 {
            end -= 1;
        }

        &self.segments[..end]
    }

    /// The upper bound of `~=`: the version with its last segment dropped
    /// and the one before bumped, so `~=3.13` is below 4 and `~=3.13.1`
    /// below 3.14.
    fn compatible_upper(&self) -> Version {
        let mut segments = self.segments.clone();

        if segments.len() > 1 {
            segments.pop();
        }

        if let Some(last) = segments.last_mut() {
            *last += 1;
        }

        Version { segments }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.segments.len().max(other.segments.len());

        for index in 0..length {
            let mine = self.segments.get(index).copied().unwrap_or(0);
            let theirs = other.segments.get(index).copied().unwrap_or(0);

            match mine.cmp(&theirs) {
                Ordering::Equal => continue,
                order => return order,
            }
        }

        Ordering::Equal
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text: Vec<String> = self.segments.iter().map(u64::to_string).collect();
        write!(f, "{}", text.join("."))
    }
}

/// One clause of a specifier set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Clause {
    /// `>=`, `>`, `<=`, `<`, `==` and `!=` against a version.
    Compare(Op, Version),
    /// `==3.13.*` and `!=3.13.*`.
    Prefix { negated: bool, prefix: Version },
    /// `~=3.13`: at least the version and below the next of its last
    /// segment.
    Compatible(Version),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
    Ne,
}

/// A PEP 440 specifier set: comma-separated clauses that all have to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecifierSet {
    clauses: Vec<Clause>,
    text: String,
}

impl SpecifierSet {
    /// Parse a specifier set such as `>=3.12,<4`. A bare version is taken
    /// as `==`, so `3.13` matches 3.13 and 3.13.x alike through padding.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut clauses = Vec::new();

        for part in text.split(',') {
            let part = part.trim();

            if part.is_empty() {
                continue;
            }

            clauses.push(parse_clause(part)?);
        }

        if clauses.is_empty() {
            return Err("an empty version specifier".to_owned());
        }

        Ok(Self {
            clauses,
            text: text.trim().to_owned(),
        })
    }

    /// Whether `version` satisfies every clause.
    pub fn contains(&self, version: &Version) -> bool {
        self.clauses.iter().all(|clause| match clause {
            Clause::Compare(op, other) => match op {
                Op::Ge => version >= other,
                Op::Gt => version > other,
                Op::Le => version <= other,
                Op::Lt => version < other,
                Op::Eq => version == other,
                Op::Ne => version != other,
            },
            Clause::Prefix { negated, prefix } => version.starts_with(prefix) != *negated,
            Clause::Compatible(lower) => version >= lower && *version < lower.compatible_upper(),
        })
    }
}

impl fmt::Display for SpecifierSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn parse_clause(part: &str) -> Result<Clause, String> {
    let operators: [(&str, Option<Op>); 8] = [
        ("===", Some(Op::Eq)),
        ("~=", None),
        (">=", Some(Op::Ge)),
        ("<=", Some(Op::Le)),
        ("==", Some(Op::Eq)),
        ("!=", Some(Op::Ne)),
        (">", Some(Op::Gt)),
        ("<", Some(Op::Lt)),
    ];

    for (symbol, op) in operators {
        let Some(rest) = part.strip_prefix(symbol) else {
            continue;
        };
        let rest = rest.trim();

        if let Some(prefix) = rest.strip_suffix(".*") {
            let negated = match op {
                Some(Op::Eq) => false,
                Some(Op::Ne) => true,
                _ => return Err(format!("{part}: a wildcard needs == or !=")),
            };

            let prefix = Version::parse(prefix).ok_or_else(|| format!("{part}: not a version"))?;
            return Ok(Clause::Prefix { negated, prefix });
        }

        let version = Version::parse(rest).ok_or_else(|| format!("{part}: not a version"))?;

        return Ok(match op {
            Some(op) => Clause::Compare(op, version),
            None => Clause::Compatible(version),
        });
    }

    let version = Version::parse(part).ok_or_else(|| format!("{part}: not a version"))?;
    Ok(Clause::Compare(Op::Eq, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_parse_and_compare_with_padding() {
        assert_eq!(version("3.13"), version("3.13.0"));
        assert!(version("3.13.1") > version("3.13"));
        assert!(version("3.9") < version("3.11"));
        assert_eq!(version("3.14.0rc1").to_string(), "3.14.0");
        assert_eq!(version("3.14t").minor(), 14);
        assert!(Version::parse("three").is_none());
        assert!(Version::parse("3.13.x").is_none());
        assert!(Version::parse("").is_none());
    }

    #[test]
    fn specifier_sets_match_as_pep_440_says() {
        let set = SpecifierSet::parse(">=3.12, <4").unwrap();
        assert!(set.contains(&version("3.12")));
        assert!(set.contains(&version("3.15.2")));
        assert!(!set.contains(&version("3.11.9")));
        assert!(!set.contains(&version("4.0")));

        let wildcard = SpecifierSet::parse("==3.13.*").unwrap();
        assert!(wildcard.contains(&version("3.13.7")));
        assert!(!wildcard.contains(&version("3.14")));

        let compatible = SpecifierSet::parse("~=3.12").unwrap();
        assert!(compatible.contains(&version("3.12")));
        assert!(compatible.contains(&version("3.99")));
        assert!(!compatible.contains(&version("4.0")));

        let patch_compatible = SpecifierSet::parse("~=3.12.1").unwrap();
        assert!(patch_compatible.contains(&version("3.12.9")));
        assert!(!patch_compatible.contains(&version("3.13")));

        let bare = SpecifierSet::parse("3.13").unwrap();
        assert!(bare.contains(&version("3.13.0")));
        assert!(!bare.contains(&version("3.13.1")));

        let excluded = SpecifierSet::parse(">=3.11,!=3.12.*").unwrap();
        assert!(excluded.contains(&version("3.11")));
        assert!(!excluded.contains(&version("3.12.3")));
    }

    #[test]
    fn specifier_errors_name_the_clause() {
        assert_eq!(
            SpecifierSet::parse(">=three").unwrap_err(),
            ">=three: not a version"
        );
        assert_eq!(
            SpecifierSet::parse(">3.13.*").unwrap_err(),
            ">3.13.*: a wildcard needs == or !="
        );
        assert!(SpecifierSet::parse("").is_err());
    }
}
