//! The builtin command database: what the server knows about Tcl and Tk itself.
//!
//! The content is generated from the official man pages shipped with the very
//! Tcl/Tk build the server is compiled against, so it is version-accurate by
//! construction rather than hand-maintained. See `crates/tcl-cmddb`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One documented command.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Command {
    pub name: String,
    /// One-line summary, from the man page's NAME section.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Call signature, from SYNOPSIS. `?x?` marks an optional argument.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synopsis: Vec<String>,
    /// Opening prose of DESCRIPTION, as markdown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Ensemble subcommands, e.g. `string cat`, `dict get`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subcommands: Vec<Subcommand>,
    /// Tk widget options, from the man page's `.OP` entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OptionSpec>,
    /// `tcl` or `tk`.
    #[serde(default)]
    pub package: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Subcommand {
    /// Just the subcommand word, e.g. `cat` for `string cat`.
    pub name: String,
    /// The full signature line as documented.
    pub signature: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub doc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OptionSpec {
    /// The `-option` flag as written in Tcl.
    pub flag: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub db_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub db_class: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub doc: String,
}

/// The whole database for one Tcl/Tk version.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Kb {
    pub version: String,
    pub commands: Vec<Command>,
    #[serde(skip)]
    by_name: HashMap<String, usize>,
}

/// The databases are generated from the man pages of the pinned Tcl/Tk builds and
/// committed, so a plain `cargo build` needs neither Tcl nor Nix. Regenerate with
/// `nix run .#regen-cmddb`.
const TCL86: &str = include_str!("../data/tcl86.json");
const TCL90: &str = include_str!("../data/tcl90.json");

/// Which command set to analyse against. This is independent of the libtcl the
/// server is *linked* against: 8.6 and 9.0 parse alike, so a binary built against
/// 8.6 can correctly analyse a project targeting 9.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    #[default]
    Tcl86,
    Tcl90,
}

impl Target {
    pub fn parse(s: &str) -> Target {
        if s.starts_with('9') {
            Target::Tcl90
        } else {
            Target::Tcl86
        }
    }
}

/// Loads the builtin database for a target. Panics only on a corrupt build.
pub fn builtin(target: Target) -> Kb {
    let json = match target {
        Target::Tcl86 => TCL86,
        Target::Tcl90 => TCL90,
    };
    Kb::from_json(json).expect("the embedded command database must be valid JSON")
}

impl Kb {
    pub fn new(version: impl Into<String>, commands: Vec<Command>) -> Kb {
        let mut kb = Kb {
            version: version.into(),
            commands,
            by_name: HashMap::new(),
        };
        kb.reindex();
        kb
    }

    pub fn from_json(text: &str) -> Result<Kb, serde_json::Error> {
        let mut kb: Kb = serde_json::from_str(text)?;
        kb.reindex();
        Ok(kb)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn reindex(&mut self) {
        self.by_name = self
            .commands
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
    }

    pub fn get(&self, name: &str) -> Option<&Command> {
        // Builtins live in the global namespace, so a leading `::` is irrelevant.
        let name = name.strip_prefix("::").unwrap_or(name);
        self.by_name.get(name).map(|i| &self.commands[*i])
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Command> {
        self.commands.iter()
    }

    /// Looks up `string cat` style ensemble subcommands.
    pub fn subcommand(&self, command: &str, sub: &str) -> Option<&Subcommand> {
        self.get(command)?
            .subcommands
            .iter()
            .find(|s| s.name == sub)
    }
}

impl Command {
    /// Markdown suitable for a hover popup.
    pub fn hover_markdown(&self) -> String {
        let mut md = String::new();
        if !self.synopsis.is_empty() {
            md.push_str("```tcl\n");
            for line in &self.synopsis {
                md.push_str(line);
                md.push('\n');
            }
            md.push_str("```\n");
        } else {
            md.push_str(&format!("```tcl\n{}\n```\n", self.name));
        }
        if !self.summary.is_empty() {
            md.push_str(&format!("\n**{}**\n", self.summary));
        }
        if !self.description.is_empty() {
            md.push('\n');
            md.push_str(&self.description);
            md.push('\n');
        }
        if !self.subcommands.is_empty() {
            let names: Vec<&str> = self
                .subcommands
                .iter()
                .map(|s| s.name.as_str())
                .take(24)
                .collect();
            md.push_str(&format!("\nSubcommands: `{}`\n", names.join("`, `")));
        }
        md
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Kb {
        let mut kb = Kb {
            version: "8.6".into(),
            commands: vec![
                Command {
                    name: "lsort".into(),
                    summary: "Sort the elements of a list".into(),
                    synopsis: vec!["lsort ?options? list".into()],
                    package: "tcl".into(),
                    ..Default::default()
                },
                Command {
                    name: "string".into(),
                    subcommands: vec![Subcommand {
                        name: "cat".into(),
                        signature: "string cat ?string1? ?string2...?".into(),
                        doc: String::new(),
                    }],
                    package: "tcl".into(),
                    ..Default::default()
                },
            ],
            by_name: HashMap::new(),
        };
        kb.reindex();
        kb
    }

    #[test]
    fn looks_up_by_name() {
        let kb = sample();
        assert_eq!(
            kb.get("lsort").unwrap().summary,
            "Sort the elements of a list"
        );
    }

    #[test]
    fn ignores_a_leading_global_qualifier() {
        assert!(sample().get("::lsort").is_some());
    }

    #[test]
    fn looks_up_ensemble_subcommands() {
        let kb = sample();
        assert_eq!(
            kb.subcommand("string", "cat").unwrap().signature,
            "string cat ?string1? ?string2...?"
        );
        assert!(kb.subcommand("string", "nope").is_none());
    }

    #[test]
    fn hover_includes_signature_and_summary() {
        let kb = sample();
        let md = kb.get("lsort").unwrap().hover_markdown();
        assert!(md.contains("lsort ?options? list"));
        assert!(md.contains("Sort the elements of a list"));
    }

    #[test]
    fn embedded_tcl86_database_loads() {
        let kb = builtin(Target::Tcl86);
        assert!(
            kb.len() > 150,
            "expected a substantial database, got {}",
            kb.len()
        );
        let lsort = kb.get("lsort").expect("lsort must be documented");
        assert!(lsort.summary.to_lowercase().contains("sort"));
        assert!(!lsort.synopsis.is_empty());
    }

    #[test]
    fn embedded_database_has_ensembles_and_tk_options() {
        let kb = builtin(Target::Tcl86);
        assert!(
            kb.subcommand("string", "cat").is_some(),
            "string ensemble subcommands must be present"
        );
        let button = kb
            .get("ttk::button")
            .expect("ttk::button must be documented");
        assert!(
            button.options.iter().any(|o| o.flag == "-command"),
            "Tk widget options must be present"
        );
    }

    #[test]
    fn tcl90_database_loads_and_differs() {
        let a = builtin(Target::Tcl86);
        let b = builtin(Target::Tcl90);
        assert!(b.len() > 150);
        assert_ne!(
            a.len(),
            b.len(),
            "the two versions document different commands"
        );
    }

    #[test]
    fn target_parses_from_a_version_string() {
        assert_eq!(Target::parse("9.0"), Target::Tcl90);
        assert_eq!(Target::parse("8.6"), Target::Tcl86);
        assert_eq!(Target::parse(""), Target::Tcl86);
    }

    #[test]
    fn json_roundtrips_and_reindexes() {
        let kb = sample();
        let back = Kb::from_json(&kb.to_json()).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back.get("lsort").is_some(), "lookup index must be rebuilt");
    }
}
