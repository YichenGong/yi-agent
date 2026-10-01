use std::fmt::Write as _;

/// One card as the frontends display it. Deliberately flat strings: the UI
/// layers differ (ratatui vs React) and neither should re-derive semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    pub id: String,
    pub state: String,
    /// e.g. `Some("3/7 tasks")`; `None` when progress is unknown.
    pub progress: Option<String>,
    /// Evidence for a finished card: branch name, commits, verification result.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardView {
    pub switch_on: bool,
    /// `project` / `global` / `default`, for the user to see where the value came from.
    pub switch_source: &'static str,
    pub cards: Vec<CardRow>,
}

impl BoardView {
    pub fn header(&self) -> String {
        let state = if self.switch_on { "on" } else { "off" };
        format!("Superpowers 看板: {state} ({})", self.switch_source)
    }

    pub fn render_lines(&self) -> Vec<String> {
        if !self.switch_on {
            return vec![
                "Superpowers 看板 is disabled. Enable it in settings, or set \
                 \"superpowers_kanban\": true in preferences.json."
                    .to_string(),
            ];
        }
        if self.cards.is_empty() {
            return vec!["Superpowers 看板 is empty.".to_string()];
        }
        self.cards
            .iter()
            .map(|card| {
                let mut line = format!("{}  {}", card.id, card.state);
                if let Some(progress) = &card.progress {
                    let _ = write!(line, "  ({progress})");
                }
                if !card.detail.is_empty() {
                    let _ = write!(line, "  {}", card.detail);
                }
                line
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(on: bool, cards: Vec<CardRow>) -> BoardView {
        BoardView {
            switch_on: on,
            switch_source: "project",
            cards,
        }
    }

    #[test]
    fn the_header_names_the_feature_and_shows_the_switch() {
        let header = view(true, vec![]).header();
        assert!(header.contains("Superpowers 看板"), "{header}");
        assert!(header.contains("on"), "{header}");
        assert!(header.contains("project"), "{header}");
    }

    #[test]
    fn a_disabled_board_says_so_instead_of_pretending_to_be_empty() {
        let lines = view(false, vec![]).render_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("disabled"), "{lines:?}");
    }

    #[test]
    fn cards_render_with_id_state_progress_and_detail() {
        let lines = view(
            true,
            vec![CardRow {
                id: "card-1".into(),
                state: "running".into(),
                progress: Some("3/7 tasks".into()),
                detail: "kanban/card-1-foo".into(),
            }],
        )
        .render_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("card-1"));
        assert!(lines[0].contains("running"));
        assert!(lines[0].contains("3/7 tasks"));
        assert!(lines[0].contains("kanban/card-1-foo"));
    }

    #[test]
    fn a_card_without_progress_omits_the_progress_slot() {
        let lines = view(
            true,
            vec![CardRow {
                id: "card-2".into(),
                state: "queued".into(),
                progress: None,
                detail: String::new(),
            }],
        )
        .render_lines();
        assert!(lines[0].contains("card-2"));
        assert!(!lines[0].contains("("), "no empty parens: {lines:?}");
    }

    #[test]
    fn an_enabled_board_with_no_cards_says_it_is_empty() {
        let lines = view(true, vec![]).render_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("empty"), "{lines:?}");
    }

    #[test]
    fn cards_keep_their_order() {
        let rows = vec![
            CardRow {
                id: "first".into(),
                state: "running".into(),
                progress: None,
                detail: String::new(),
            },
            CardRow {
                id: "second".into(),
                state: "queued".into(),
                progress: None,
                detail: String::new(),
            },
        ];
        let lines = view(true, rows).render_lines();
        assert!(lines[0].contains("first"));
        assert!(lines[1].contains("second"));
    }
}
