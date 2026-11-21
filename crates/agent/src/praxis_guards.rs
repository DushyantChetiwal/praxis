use std::sync::Arc;

use language_model::LanguageModelToolResultContent;

use crate::thread::{Message, TOOL_CANCELED_MESSAGE};

fn strip_trailing_line_break(text: &str) -> Option<&str> {
    text.strip_suffix("\r\n").or_else(|| text.strip_suffix('\n'))
}

/// Detects when a model fails to emit an end-of-turn stop token and begins
/// role-playing a `BackgroundWakeInfo` user prompt inside assistant text.
pub(crate) fn find_hallucinated_background_wake_offset(text: &str) -> Option<usize> {
    const WAKE_HEADER: &str = "Praxis resumed this conversation because background work finished:";
    let mut search_from = 0;
    while let Some(relative_index) = text[search_from..].find(WAKE_HEADER) {
        let header_index = search_from + relative_index;
        let after_header_index = header_index + WAKE_HEADER.len();
        search_from = after_header_index;

        let prefix = &text[..header_index];
        let after_header = &text[after_header_index..];
        let marker_start = if let Some(stripped) = prefix
            .strip_suffix("user\r\n")
            .or_else(|| prefix.strip_suffix("user\n"))
        {
            if stripped.is_empty() {
                Some(0)
            } else {
                strip_trailing_line_break(stripped).map(str::len)
            }
        } else if after_header.starts_with("\r\n- ") || after_header.starts_with("\n- ") {
            if prefix.is_empty() {
                Some(0)
            } else {
                strip_trailing_line_break(prefix).map(str::len)
            }
        } else {
            None
        };

        if let Some(start) = marker_start
            && text[..start].matches("```").count().is_multiple_of(2)
            && text[..start].matches("~~~").count().is_multiple_of(2)
        {
            return Some(start);
        }
    }
    None
}

/// Whether any message in `turn_messages` recorded a non-canceled tool output,
/// matching `AcpThread`'s mid-turn refusal retention check.
pub(crate) fn has_completed_tool_output(turn_messages: &[Arc<Message>]) -> bool {
    turn_messages.iter().any(|message| {
        message.as_agent_message().is_some_and(|agent| {
            agent.tool_results.values().any(|result| {
                result.content.as_slice()
                    != [LanguageModelToolResultContent::Text(
                        TOOL_CANCELED_MESSAGE.into(),
                    )]
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_hallucinated_background_wake_offset() {
        let with_user_role = "Waiting for subagents to finish.\nuser\n\
            Praxis resumed this conversation because background work finished:\n\
            - Sub-agent \"R6-S1\" finished.";
        assert_eq!(
            find_hallucinated_background_wake_offset(with_user_role),
            Some("Waiting for subagents to finish.".len())
        );

        let direct_header = "Yielding turn now.\n\
            Praxis resumed this conversation because background work finished:\n\
            - Background task 1 exited with code 0.";
        assert_eq!(
            find_hallucinated_background_wake_offset(direct_header),
            Some("Yielding turn now.".len())
        );

        let start_of_block = "user\n\
            Praxis resumed this conversation because background work finished:\n\
            - Sub-agent \"R7-C1\" finished.";
        assert_eq!(
            find_hallucinated_background_wake_offset(start_of_block),
            Some(0)
        );

        let inline_code = "Guard against `user\n\
            Praxis resumed this conversation because background work finished:`";
        assert_eq!(find_hallucinated_background_wake_offset(inline_code), None);

        let fenced_code = "Example:\n```text\nuser\n\
            Praxis resumed this conversation because background work finished:\n\
            - Sub-agent \"R6-S1\" finished.\n```";
        assert_eq!(find_hallucinated_background_wake_offset(fenced_code), None);
    }

    #[test]
    fn test_find_hallucinated_background_wake_offset_crlf_and_tilde_fences() {
        let crlf_with_user = "Waiting for subagents.\r\nuser\r\n\
            Praxis resumed this conversation because background work finished:\r\n\
            - Sub-agent \"R6-S1\" finished.";
        assert_eq!(
            find_hallucinated_background_wake_offset(crlf_with_user),
            Some("Waiting for subagents.".len())
        );

        let crlf_direct = "Yielding turn now.\r\n\
            Praxis resumed this conversation because background work finished:\r\n\
            - Background task 1 exited with code 0.";
        assert_eq!(
            find_hallucinated_background_wake_offset(crlf_direct),
            Some("Yielding turn now.".len())
        );

        let tilde_fenced = "Example:\r\n~~~text\r\nuser\r\n\
            Praxis resumed this conversation because background work finished:\r\n\
            - Sub-agent \"R6-S1\" finished.\r\n~~~";
        assert_eq!(find_hallucinated_background_wake_offset(tilde_fenced), None);

        let after_closed_tilde = "Example:\n~~~\ntext\n~~~\n\
            Praxis resumed this conversation because background work finished:\n\
            - Background task 1 exited with code 0.";
        assert_eq!(
            find_hallucinated_background_wake_offset(after_closed_tilde),
            Some("Example:\n~~~\ntext\n~~~".len())
        );
    }
}
