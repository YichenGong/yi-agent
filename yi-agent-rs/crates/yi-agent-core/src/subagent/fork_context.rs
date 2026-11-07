//! Task-boundary marker for a forked child's inherited transcript.
//!
//! A forked child is seeded with its parent's conversation so it can pick up
//! where the parent left off. Without a marker, that history is
//! indistinguishable from the child's own: the model reads the parent's last
//! unfinished step as *its* next step and continues the parent's task instead
//! of the objective it was actually given.
//!
//! The fix is positional, not instructional: close the inherited history with
//! one message that marks everything before it as reference. The child's own
//! objective is appended after it, so the ordering itself carries the signal.

use crate::message::Message;

/// Marks the inherited transcript as background, never as the child's task.
///
/// Written in the second person and placed *after* the inherited history, so it
/// reads as a statement about what came before it. Naming the parent's material
/// explicitly ("以下是父会话的历史记录") is what separates the marker from the
/// parent's own last turn: without that phrasing the model treats the tail of
/// the transcript as its own pending work.
pub const SUBAGENT_FORK_BOUNDARY: &str = "\
以下内容是父会话的历史记录，仅作为只读背景供你理解上下文。

它们不是你的任务：其中的用户请求、计划、审批、工具调用与输出都属于父会话，已经完成或已不再相关。不要继续执行父会话的未完成步骤，不要把它最后那个动作当作你该接着做的下一步。

你真正的任务是这条消息之后、由派发者给你的那个目标。只针对那个目标执行。";

/// Seeds a forked child with `parent` closed by the boundary marker.
///
/// The marker is a user turn: the child's objective is appended as the next
/// user turn by [`crate::Agent::run`], so both are user-role blocks the model
/// reads in order. An assistant-role marker would make the objective look like
/// a continuation of the parent's reasoning rather than a fresh instruction.
///
/// The history itself is copied verbatim. Rewriting it would change what the
/// parent did into something the parent did not say.
///
/// An empty `parent` yields an empty seed: a marker with no history above it
/// would describe a background that does not exist, so the child starts clean
/// instead of being told to ignore nothing.
pub fn forked_seed(parent: Vec<Message>) -> Vec<Message> {
    if parent.is_empty() {
        return parent;
    }
    let mut seed = parent;
    seed.push(Message::user(SUBAGENT_FORK_BOUNDARY));
    seed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Role};

    fn parent_transcript() -> Vec<Message> {
        vec![
            Message::user("parent asked for something"),
            Message::assistant(vec![ContentBlock::Text("parent was working".into())]),
        ]
    }

    #[test]
    fn seed_keeps_the_inherited_history_in_order() {
        let parent = parent_transcript();
        let seed = forked_seed(parent.clone());

        assert_eq!(
            &seed[..parent.len()],
            parent.as_slice(),
            "inherited history must be copied verbatim, in order"
        );
    }

    #[test]
    fn seed_closes_the_history_with_a_non_empty_boundary() {
        let seed = forked_seed(parent_transcript());
        let last = seed.last().expect("seed is never empty");

        assert_eq!(
            last.content,
            vec![ContentBlock::Text(SUBAGENT_FORK_BOUNDARY.to_string())],
            "the inherited history must end on the boundary marker"
        );
        assert!(
            !SUBAGENT_FORK_BOUNDARY.trim().is_empty(),
            "an empty marker would signal nothing to the model"
        );
    }

    #[test]
    fn boundary_is_a_user_turn_so_the_objective_still_follows_as_user() {
        let seed = forked_seed(parent_transcript());
        let boundary = seed.last().expect("seed is never empty");

        assert_eq!(
            boundary.role,
            Role::User,
            "a user-role marker keeps the objective a second user turn; an \
             assistant marker would force the objective to be read as tool \
             output or a continuation"
        );
    }

    #[test]
    fn seed_adds_exactly_one_message() {
        let parent = parent_transcript();
        let seed = forked_seed(parent.clone());

        assert_eq!(
            seed.len(),
            parent.len() + 1,
            "the marker is the only addition; the history is not rewritten"
        );
    }

    #[test]
    fn boundary_does_not_claim_the_objective_belongs_to_the_parent() {
        let seed = forked_seed(parent_transcript());
        let boundary = match &seed.last().expect("seed is never empty").content[0] {
            ContentBlock::Text(text) => text.clone(),
            other => panic!("boundary must be plain text, got {other:?}"),
        };

        assert!(
            boundary.contains("不是") || boundary.contains("not"),
            "the marker must state that the history above it is not the task: {boundary}"
        );
    }

    #[test]
    fn an_empty_parent_transcript_gets_no_boundary() {
        // Nothing above the marker means the marker describes history that is
        // not there. A child with no inherited context should start clean, not
        // be told to ignore a background that does not exist.
        assert!(
            forked_seed(Vec::new()).is_empty(),
            "an empty history needs no boundary and must not gain a lone marker"
        );
    }
}
