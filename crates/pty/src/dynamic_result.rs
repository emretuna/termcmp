use std::fmt;

use suggest::Suggestion;

#[derive(Debug, Clone)]
pub enum DynamicResult {
    Loaded {
        provider: ProviderTag,
        suggestions: Vec<Suggestion>,
    },
    Empty {
        provider: ProviderTag,
    },
    Error {
        provider: ProviderTag,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderTag {
    /// Async providers (LLM, fish/zsh shell-native completions).
    Async(String),
}

impl fmt::Display for ProviderTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Async(name) => write!(f, "{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use suggest::SuggestionKind;

    #[test]
    fn provider_tag_display_is_stable() {
        assert_eq!(ProviderTag::Async("llm".into()).to_string(), "llm");
    }

    #[test]
    fn dynamic_result_variants_carry_payloads() {
        let suggestion = Suggestion {
            text: "main".into(),
            kind: SuggestionKind::Subcommand,
            ..Default::default()
        };
        let result = DynamicResult::Loaded {
            provider: ProviderTag::Async("test".into()),
            suggestions: vec![suggestion],
        };
        match result {
            DynamicResult::Loaded { suggestions, .. } => assert_eq!(suggestions.len(), 1),
            _ => panic!("expected loaded"),
        }
    }
}
