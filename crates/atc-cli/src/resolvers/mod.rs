pub mod prompt;
pub mod task;
pub mod template;

use atc_core::config::AtcConfig;
use atc_core::resolver::InputResolver;

/// Single source of truth: instantiate a resolver by name.
///
/// Used by `build_resolvers` (config-driven chain) and `resolver_by_name`
/// (stop/cleanup/close/retry lookups). Add new resolvers here only.
pub fn make_resolver(name: &str) -> Option<Box<dyn InputResolver>> {
    match name {
        "task" => Some(Box::new(task::TaskResolver)),
        "template" => Some(Box::new(template::TemplateResolver)),
        "prompt" => Some(Box::new(prompt::PromptResolver)),
        _ => None,
    }
}

/// Build the resolver chain from config, respecting order and enabled flags.
pub fn build_resolvers(config: &AtcConfig) -> Vec<Box<dyn InputResolver>> {
    let rc = &config.resolvers;
    let mut resolvers: Vec<Box<dyn InputResolver>> = Vec::new();

    for name in &rc.order {
        let enabled = match name.as_str() {
            "task" => rc.task.enabled,
            "template" => rc.template.enabled,
            "prompt" => rc.prompt.enabled,
            _ => {
                tracing::warn!(resolver = %name, "unknown resolver name in [resolvers].order; skipping");
                false
            }
        };
        if enabled {
            if let Some(r) = make_resolver(name) {
                resolvers.push(r);
            }
        }
    }

    resolvers
}

#[cfg(test)]
mod tests {
    use super::*;
    use atc_core::config::ResolverEntryConfig;

    #[test]
    fn test_build_resolvers_default_order() {
        let config = AtcConfig::default();
        let resolvers = build_resolvers(&config);
        assert_eq!(resolvers.len(), 3);
        assert_eq!(resolvers[0].name(), "task");
        assert_eq!(resolvers[1].name(), "template");
        assert_eq!(resolvers[2].name(), "prompt");
    }

    #[test]
    fn test_build_resolvers_task_disabled() {
        let mut config = AtcConfig::default();
        config.resolvers.task = ResolverEntryConfig { enabled: false };
        let resolvers = build_resolvers(&config);
        assert_eq!(resolvers.len(), 2);
        assert_eq!(resolvers[0].name(), "template");
        assert_eq!(resolvers[1].name(), "prompt");
    }

    #[test]
    fn test_build_resolvers_custom_order() {
        let mut config = AtcConfig::default();
        config.resolvers.order = vec!["prompt".to_string(), "template".to_string()];
        let resolvers = build_resolvers(&config);
        assert_eq!(resolvers.len(), 2);
        assert_eq!(resolvers[0].name(), "prompt");
        assert_eq!(resolvers[1].name(), "template");
    }

    #[test]
    fn test_build_resolvers_all_disabled() {
        let mut config = AtcConfig::default();
        config.resolvers.task = ResolverEntryConfig { enabled: false };
        config.resolvers.template = ResolverEntryConfig { enabled: false };
        config.resolvers.prompt = ResolverEntryConfig { enabled: false };
        let resolvers = build_resolvers(&config);
        assert!(resolvers.is_empty());
    }

    #[test]
    fn test_build_resolvers_unknown_name_skipped() {
        let mut config = AtcConfig::default();
        config.resolvers.order = vec![
            "nonexistent".to_string(),
            "task".to_string(),
            "typo".to_string(),
        ];
        let resolvers = build_resolvers(&config);
        assert_eq!(resolvers.len(), 1);
        assert_eq!(resolvers[0].name(), "task");
    }
}
