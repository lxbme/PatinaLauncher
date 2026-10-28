//! Library / argument rules.

use std::collections::{HashMap, HashSet};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Disallow,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsRule {
    pub name: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: RuleAction,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEnv {
    pub os_name: String,
    pub arch: String,
    pub os_version: String,
    pub features: HashSet<String>,
}

impl RuleEnv {
    pub fn current() -> Self {
        RuleEnv {
            os_name: platform::rule_os_name().to_string(),
            arch: platform::rule_arch().to_string(),
            os_version: platform::os_version(),
            features: HashSet::new(),
        }
    }
}

fn rule_matches(rule: &Rule, env: &RuleEnv) -> bool {
    if let Some(os) = &rule.os {
        if os.name.as_deref().is_some_and(|n| n != env.os_name) {
            return false;
        }
        if os.arch.as_deref().is_some_and(|a| a != env.arch) {
            return false;
        }
        if let Some(v) = &os.version {
            // An invalid pattern never matches.
            match Regex::new(v) {
                Ok(re) if re.is_match(&env.os_version) => {}
                _ => return false,
            }
        }
    }
    if let Some(features) = &rule.features
        && features.iter().any(|(k, v)| env.features.contains(k) != *v)
    {
        return false;
    }
    true
}

/// Empty rule list allows. Otherwise start disallowed; every matching rule sets the result to its action.
pub fn rules_allow(rules: &[Rule], env: &RuleEnv) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for r in rules {
        if rule_matches(r, env) {
            allowed = r.action == RuleAction::Allow;
        }
    }
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> RuleEnv {
        RuleEnv {
            os_name: "linux".into(),
            arch: "x86_64".into(),
            os_version: "6.0".into(),
            features: HashSet::new(),
        }
    }

    fn rules(json: &str) -> Vec<Rule> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn rule_semantics() {
        let env = linux();
        assert!(rules_allow(&[], &env));
        assert!(!rules_allow(
            &rules(r#"[{"action":"allow","os":{"name":"osx"}}]"#),
            &env
        ));
        assert!(rules_allow(
            &rules(r#"[{"action":"allow"},{"action":"disallow","os":{"name":"osx"}}]"#),
            &env
        ));
        assert!(rules_allow(
            &rules(r#"[{"action":"allow","os":{"name":"linux","version":"^6\\."}}]"#),
            &env
        ));
        assert!(!rules_allow(
            &rules(r#"[{"action":"allow","os":{"arch":"x86"}}]"#),
            &env
        ));
        assert!(!rules_allow(
            &rules(r#"[{"action":"allow","features":{"is_demo_user":true}}]"#),
            &env
        ));
        assert!(!rules_allow(
            &rules(r#"[{"action":"disallow","os":{"name":"linux"}}]"#),
            &env
        ));
    }

    #[test]
    fn features_and_false_values() {
        let mut env = linux();
        let demo = rules(r#"[{"action":"allow","features":{"is_demo_user":true}}]"#);
        env.features.insert("is_demo_user".into());
        assert!(rules_allow(&demo, &env));
        let not_demo = rules(r#"[{"action":"allow","features":{"is_demo_user":false}}]"#);
        assert!(!rules_allow(&not_demo, &env));
    }

    #[test]
    fn current_env_is_populated() {
        let e = RuleEnv::current();
        assert!(!e.os_name.is_empty() && !e.arch.is_empty());
        assert!(e.features.is_empty());
    }
}
