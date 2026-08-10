//! Server configuration.
//!
//! Settings arrive from three places, in increasing precedence: environment
//! variables (which the Nix wrapper sets to absolute store paths, as defaults),
//! `initializationOptions`, and `workspace/didChangeConfiguration`. Everything the
//! server can be told lives here, so no handler reaches for `std::env` directly.

use serde_json::Value;
use tcl_analysis::kb;

/// An external analyser: where it lives, and whether to run it.
#[derive(Debug, Clone)]
pub struct Tool {
    pub path: String,
    pub enabled: bool,
}

impl Tool {
    fn from_env(var: &str, fallback: &str) -> Tool {
        Tool {
            path: std::env::var(var).unwrap_or_else(|_| fallback.to_string()),
            enabled: !matches!(
                std::env::var(format!("{var}_ENABLE")).ok().as_deref(),
                Some("0") | Some("false") | Some("off")
            ),
        }
    }

    /// The path to run, or `None` when the tool is switched off.
    pub fn active(&self) -> Option<&str> {
        self.enabled.then_some(self.path.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Which command set to analyse against. Independent of the libtcl this binary
    /// links to: 8.6 and 9.0 parse alike, so an 8.6 build can analyse a 9.0 project.
    pub tcl_version: kb::Target,
    pub nagelfar: Tool,
    /// Syntax database for nagelfar, which cannot run without one.
    pub nagelfar_db: Option<String>,
    pub tclint: Tool,
    pub tclfmt: Tool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tcl_version: std::env::var("TCL_LSP_TCL_VERSION")
                .map(|v| kb::Target::parse(&v))
                .unwrap_or_default(),
            nagelfar: Tool::from_env("TCL_LSP_NAGELFAR", "nagelfar"),
            nagelfar_db: std::env::var("TCL_LSP_NAGELFAR_DB").ok(),
            tclint: Tool::from_env("TCL_LSP_TCLINT", "tclint"),
            tclfmt: Tool::from_env("TCL_LSP_TCLFMT", "tclfmt"),
        }
    }
}

impl Config {
    /// Applies a settings object, which may be the whole `settings` payload or
    /// just the `tclLsp` section. Absent keys are left untouched, so a client that
    /// sends a partial update does not reset everything else.
    ///
    /// Returns whether the analysis target changed, since that means the builtin
    /// command database has to be reloaded.
    pub fn merge(&mut self, settings: &Value) -> bool {
        let root = settings.get("tclLsp").unwrap_or(settings);
        let before = self.tcl_version;

        if let Some(v) = root.get("tclVersion").and_then(Value::as_str) {
            self.tcl_version = kb::Target::parse(v);
        }
        merge_tool(&mut self.nagelfar, root.get("nagelfar"));
        merge_tool(&mut self.tclint, root.get("tclint"));
        merge_tool(&mut self.tclfmt, root.get("tclfmt"));
        if let Some(db) = root
            .get("nagelfar")
            .and_then(|n| n.get("syntaxDb"))
            .and_then(Value::as_str)
        {
            self.nagelfar_db = Some(db.to_string());
        }
        // Also accept the flat `diagnostics.nagelfar = false` shorthand.
        if let Some(d) = root.get("diagnostics") {
            if let Some(on) = d.get("nagelfar").and_then(Value::as_bool) {
                self.nagelfar.enabled = on;
            }
            if let Some(on) = d.get("tclint").and_then(Value::as_bool) {
                self.tclint.enabled = on;
            }
        }
        before != self.tcl_version
    }
}

fn merge_tool(tool: &mut Tool, value: Option<&Value>) {
    let Some(v) = value else { return };
    // A bare boolean or string is a convenient shorthand for the common cases.
    match v {
        Value::Bool(on) => tool.enabled = *on,
        Value::String(path) => tool.path = path.clone(),
        Value::Object(_) => {
            if let Some(p) = v.get("path").and_then(Value::as_str) {
                tool.path = p.to_string();
            }
            if let Some(on) = v.get("enable").and_then(Value::as_bool) {
                tool.enabled = on;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Config {
        Config {
            tcl_version: kb::Target::Tcl86,
            nagelfar: Tool {
                path: "nagelfar".into(),
                enabled: true,
            },
            nagelfar_db: None,
            tclint: Tool {
                path: "tclint".into(),
                enabled: true,
            },
            tclfmt: Tool {
                path: "tclfmt".into(),
                enabled: true,
            },
        }
    }

    #[test]
    fn accepts_the_tcl_lsp_section_or_a_bare_object() {
        let mut a = base();
        assert!(a.merge(&json!({"tclLsp": {"tclVersion": "9.0"}})));
        assert_eq!(a.tcl_version, kb::Target::Tcl90);

        let mut b = base();
        assert!(b.merge(&json!({"tclVersion": "9.0"})));
        assert_eq!(b.tcl_version, kb::Target::Tcl90);
    }

    #[test]
    fn reports_whether_the_analysis_target_moved() {
        let mut c = base();
        assert!(!c.merge(&json!({"tclVersion": "8.6"})), "same version");
        assert!(c.merge(&json!({"tclVersion": "9.0"})), "changed");
    }

    #[test]
    fn a_partial_update_leaves_other_settings_alone() {
        let mut c = base();
        c.tclint.enabled = false;
        c.merge(&json!({"tclVersion": "9.0"}));
        assert!(!c.tclint.enabled, "unrelated settings must survive");
    }

    #[test]
    fn tools_accept_boolean_string_and_object_forms() {
        let mut c = base();
        c.merge(&json!({"nagelfar": false}));
        assert!(!c.nagelfar.enabled);

        c.merge(&json!({"tclint": "/usr/bin/tclint"}));
        assert_eq!(c.tclint.path, "/usr/bin/tclint");

        c.merge(&json!({"tclfmt": {"path": "/opt/tclfmt", "enable": false}}));
        assert_eq!(c.tclfmt.path, "/opt/tclfmt");
        assert!(!c.tclfmt.enabled);
    }

    #[test]
    fn the_diagnostics_shorthand_toggles_backends() {
        let mut c = base();
        c.merge(&json!({"diagnostics": {"nagelfar": false, "tclint": false}}));
        assert!(!c.nagelfar.enabled);
        assert!(!c.tclint.enabled);
    }

    #[test]
    fn a_disabled_tool_reports_no_path_to_run() {
        let mut c = base();
        assert_eq!(c.nagelfar.active(), Some("nagelfar"));
        c.merge(&json!({"nagelfar": false}));
        assert_eq!(c.nagelfar.active(), None);
    }

    #[test]
    fn the_nagelfar_syntax_database_is_configurable() {
        let mut c = base();
        c.merge(&json!({"nagelfar": {"syntaxDb": "/db/syntaxdb90.tcl"}}));
        assert_eq!(c.nagelfar_db.as_deref(), Some("/db/syntaxdb90.tcl"));
    }
}
