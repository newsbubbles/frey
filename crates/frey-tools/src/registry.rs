//! Where the agent finds out what it can do.
//!
//! The registry holds every capability from every source under one namespace, and enforces the
//! rules that stop a catalog from quietly breaking the prompt cache or the model's ability to find
//! anything:
//!
//! * **names are unique**, and a collision is an error at registration rather than a mystery at
//!   runtime;
//! * **listing order is deterministic**, because the tool block is the stable cache prefix and a
//!   source that reorders its listing would rewrite it every turn;
//! * **discoverability is measurable**, so a tool nobody can find is a reportable defect.

use std::collections::BTreeMap;

use frey_core::ids::ToolName;
use frey_core::tool_def::{Discoverability, PresentationHint, ToolDefinition};
use smol_str::SmolStr;

/// A capability could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// Two sources offered the same name.
    #[error(
        "two tools are both called `{name}` (from `{first}` and `{second}`). Namespace one of \
         them, e.g. `{second}_{name}`, so the model can tell them apart."
    )]
    DuplicateName {
        /// The contested name.
        name: ToolName,
        /// Who registered it first.
        first: SmolStr,
        /// Who tried to register it second.
        second: SmolStr,
    },
}

/// Every capability the agent has, from every source.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    // A `BTreeMap` rather than a `HashMap`: iteration order must be stable across processes, or
    // the tool block's hash changes between runs and every cold start pays to rebuild the cache.
    entries: BTreeMap<ToolName, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    definition: ToolDefinition,
    source: SmolStr,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a definition from `source`.
    ///
    /// # Errors
    /// Returns [`RegistryError::DuplicateName`] rather than letting one tool shadow another, which
    /// would present the model with a name that does something different depending on load order.
    pub fn register(
        &mut self,
        source: impl Into<SmolStr>,
        definition: ToolDefinition,
    ) -> Result<(), RegistryError> {
        let source = source.into();
        if let Some(existing) = self.entries.get(&definition.name) {
            return Err(RegistryError::DuplicateName {
                name: definition.name.clone(),
                first: existing.source.clone(),
                second: source,
            });
        }
        self.entries.insert(definition.name.clone(), Entry { definition, source });
        Ok(())
    }

    /// Add a definition under a namespace prefix, e.g. `github` + `list_issues` → `github_list_issues`.
    ///
    /// Consistent prefixes let one search match a whole service's tools, which is why namespacing
    /// is a first-class operation rather than something callers do by hand.
    ///
    /// # Errors
    /// Returns [`RegistryError::DuplicateName`] on collision.
    pub fn register_prefixed(
        &mut self,
        source: impl Into<SmolStr>,
        prefix: &str,
        mut definition: ToolDefinition,
    ) -> Result<(), RegistryError> {
        definition.name = ToolName::new(format!("{prefix}_{}", definition.name));
        self.register(source, definition)
    }

    /// Look one up.
    #[must_use]
    pub fn get(&self, name: &ToolName) -> Option<&ToolDefinition> {
        self.entries.get(name).map(|e| &e.definition)
    }

    /// Which source a tool came from.
    #[must_use]
    pub fn source_of(&self, name: &ToolName) -> Option<&str> {
        self.entries.get(name).map(|e| e.source.as_str())
    }

    /// How many capabilities are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every definition, in a deterministic order.
    pub fn all(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.entries.values().map(|e| &e.definition)
    }

    /// The definitions that should occupy the stable prefix.
    pub fn always_present(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.all().filter(|d| d.presentation == PresentationHint::Always)
    }

    /// The definitions that are only loaded when discovered.
    pub fn deferred(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.all().filter(|d| d.presentation == PresentationHint::Deferred)
    }

    /// Tools that would be hard to find, with the reasons.
    ///
    /// This is what `frey doctor` reports. A tool with no description or undocumented parameters is
    /// a real defect once a catalog outgrows the context window, not a style preference.
    #[must_use]
    pub fn discoverability_problems(&self) -> Vec<(ToolName, Vec<Discoverability>)> {
        self.all()
            .filter_map(|d| {
                let report = d.discoverability();
                if report.is_clean() { None } else { Some((d.name.clone(), report.problems)) }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::tool_def::JsonSchema;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            "A tool described well enough that a search can actually find it",
            JsonSchema::empty_object(),
        )
    }

    #[test]
    fn a_name_collision_is_an_error_that_names_both_sources() {
        let mut registry = ToolRegistry::new();
        registry.register("native", def("fs_read")).unwrap();
        let err = registry.register("mcp:files", def("fs_read")).unwrap_err();

        let message = format!("{err}");
        assert!(message.contains("native") && message.contains("mcp:files"), "{message}");
        assert!(message.contains("Namespace"), "and it says how to fix it: {message}");
    }

    #[test]
    fn listing_order_is_stable_regardless_of_registration_order() {
        // The tool block is the stable cache prefix. If its order depended on insertion or on a
        // hash seed, every process restart would pay to rebuild the cache.
        let mut a = ToolRegistry::new();
        a.register("s", def("z_last")).unwrap();
        a.register("s", def("a_first")).unwrap();

        let mut b = ToolRegistry::new();
        b.register("s", def("a_first")).unwrap();
        b.register("s", def("z_last")).unwrap();

        let names_a: Vec<_> = a.all().map(|d| d.name.clone()).collect();
        let names_b: Vec<_> = b.all().map(|d| d.name.clone()).collect();
        assert_eq!(names_a, names_b);
        assert_eq!(names_a[0], ToolName::new("a_first"));
    }

    #[test]
    fn namespacing_lets_one_search_match_a_whole_service() {
        let mut registry = ToolRegistry::new();
        registry.register_prefixed("mcp:github", "github", def("list_issues")).unwrap();
        registry.register_prefixed("mcp:github", "github", def("create_issue")).unwrap();

        let names: Vec<String> = registry.all().map(|d| d.name.to_string()).collect();
        assert_eq!(names, ["github_create_issue", "github_list_issues"]);
        assert!(registry.all().all(|d| d.namespace() == Some("github")));
    }

    #[test]
    fn presentation_partitions_the_catalog() {
        let mut registry = ToolRegistry::new();
        let mut hot = def("fs_read");
        hot.presentation = PresentationHint::Always;
        registry.register("native", hot).unwrap();
        registry.register("native", def("z_rare")).unwrap();

        assert_eq!(registry.always_present().count(), 1);
        assert_eq!(registry.deferred().count(), 1);
    }

    #[test]
    fn undiscoverable_tools_are_reported_as_defects() {
        let mut registry = ToolRegistry::new();
        registry
            .register("native", ToolDefinition::new("doit", "Does it", JsonSchema::empty_object()))
            .unwrap();
        registry.register("native", def("fs_read")).unwrap();

        let problems = registry.discoverability_problems();
        assert_eq!(problems.len(), 1, "only the thin one");
        assert_eq!(problems[0].0, ToolName::new("doit"));
        assert!(problems[0].1.contains(&Discoverability::ThinDescription { words: 2 }));
    }

    #[test]
    fn source_attribution_survives_registration() {
        let mut registry = ToolRegistry::new();
        registry.register("mcp:github", def("gh_list")).unwrap();
        assert_eq!(registry.source_of(&ToolName::new("gh_list")), Some("mcp:github"));
    }
}
