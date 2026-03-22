pub mod prompt;
pub mod task;
pub mod template;

use atc_core::config::AtcConfig;
use atc_core::resolver::InputResolver;

/// Build the resolver chain from config, respecting order and enabled flags.
pub fn build_resolvers(config: &AtcConfig) -> Vec<Box<dyn InputResolver>> {
    let rc = &config.resolvers;
    let mut resolvers: Vec<Box<dyn InputResolver>> = Vec::new();

    for name in &rc.order {
        match name.as_str() {
            "task" if rc.task.enabled => {
                resolvers.push(Box::new(task::TaskResolver));
            }
            "template" if rc.template.enabled => {
                resolvers.push(Box::new(template::TemplateResolver));
            }
            "prompt" if rc.prompt.enabled => {
                resolvers.push(Box::new(prompt::PromptResolver));
            }
            _ => {
                // Unknown or disabled resolver — skip silently
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
}
