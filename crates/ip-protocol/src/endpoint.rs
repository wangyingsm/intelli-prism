//! Where each family's completions are asked for, under an upstream's base path.
//!
//! A proxied request never needs this: its caller chose the path and the gateway carries what
//! it was given. A request the deployment makes for itself has no caller to take one from, so
//! the family has to say where it sends a conversation.

use ip_core::{Dialect, ModelName};

/// Where a conversation in this family is sent, relative to the upstream's base path.
///
/// Gemini names the model in the path rather than the body, so it needs one; a conversation
/// that pinned none is sent to the family's own default name, which is what an upstream
/// configured with a model will have filled in by the time it is asked.
#[must_use]
pub fn under(speaks: Dialect, model: Option<&ModelName>) -> String {
    match speaks {
        Dialect::OpenAi => "/chat/completions".to_owned(),
        Dialect::Anthropic => "/messages".to_owned(),
        Dialect::Gemini => format!(
            "/models/{}:generateContent",
            model.map_or("gemini-pro", ModelName::as_str)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_family_says_where_it_sends_a_conversation() {
        let model = ModelName::new("gemini-2.5-pro").unwrap();

        assert_eq!(under(Dialect::OpenAi, None), "/chat/completions");
        assert_eq!(under(Dialect::Anthropic, None), "/messages");
        assert_eq!(
            under(Dialect::Gemini, Some(&model)),
            "/models/gemini-2.5-pro:generateContent"
        );
    }

    /// The two families that carry the model in the body do not change path for it.
    #[test]
    fn naming_a_model_moves_only_the_family_that_puts_it_in_the_path() {
        let model = ModelName::new("gpt-4o").unwrap();

        assert_eq!(
            under(Dialect::OpenAi, Some(&model)),
            under(Dialect::OpenAi, None)
        );
        assert_eq!(
            under(Dialect::Anthropic, Some(&model)),
            under(Dialect::Anthropic, None)
        );
        assert_ne!(
            under(Dialect::Gemini, Some(&model)),
            under(Dialect::Gemini, None)
        );
    }

    /// Every family's path is absolute, because it is joined onto a base path.
    #[test]
    fn every_family_names_an_absolute_path() {
        for speaks in [Dialect::OpenAi, Dialect::Anthropic, Dialect::Gemini] {
            assert!(under(speaks, None).starts_with('/'), "{speaks:?}");
        }
    }
}
