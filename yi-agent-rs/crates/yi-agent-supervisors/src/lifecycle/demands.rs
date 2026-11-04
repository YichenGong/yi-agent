//! Which clients still need a shared plugin service.
//!
//! A plugin service (an MCP server, for instance) is shared: starting one copy per
//! client would multiply its cost, while starting one on behalf of *nobody* is how
//! a background process outlives its reason to exist. So the only thing this table
//! tracks is demand -- a set of clients per plugin -- and the rule is the last one
//! out stops the service.
//!
//! Two things it deliberately is not:
//!
//! - Not a lease table. Demand does not expire, renew, or authorise anything; it
//!   answers "may this plugin be stopped yet", nothing else.
//! - Not a persisted setting. A plugin that is enabled in configuration but has no
//!   client attached is *not* demand. Configuration says what is allowed to run;
//!   demand says someone is waiting for it.
//!
//! Holding demand is idempotent on purpose. A client may trigger the same plugin
//! twice (a retry, two tasks), and having to count in every caller would make a
//! single missed decrement leak a service forever.

use std::collections::{BTreeMap, BTreeSet};

use yi_agent_lifecycle::types::ClientRunId;

/// Per-plugin sets of clients that are currently waiting on it.
#[derive(Clone, Debug, Default)]
pub struct PluginDemandTable {
    /// `BTreeMap`/`BTreeSet` so `owners` answers in a stable order; callers that
    /// pick "the" owner from the list must not see it reshuffle run to run.
    demands: BTreeMap<String, BTreeSet<ClientRunId>>,
}

/// What a `release` call means for the plugin's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandRelease {
    /// Other clients still need it; leave it running.
    StillWanted,
    /// That was the last client; the service may be stopped.
    NoLongerWanted,
    /// This client held no demand for the plugin, so nothing changed.
    NotHeld,
}

impl PluginDemandTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `client` is waiting on `plugin`.
    ///
    /// Returns whether the plugin went from *nobody* waiting to somebody waiting:
    /// that, and only that, is the moment a service must be started. A second
    /// client joining an already-wanted plugin must reuse the running service
    /// rather than start another copy, so it returns `false`.
    pub fn acquire(&mut self, client: &str, plugin: &str) -> bool {
        let owners = self.demands.entry(plugin.to_string()).or_default();
        let was_wanted = !owners.is_empty();
        owners.insert(client.to_string());
        !was_wanted
    }

    /// Records that `client` is done with `plugin`.
    ///
    /// The plugin stops only when the last holder leaves. A client that never held
    /// the plugin is told so rather than counted as a release: otherwise an
    /// unbalanced release from one client could stop a service another is using.
    pub fn release(&mut self, client: &str, plugin: &str) -> DemandRelease {
        let Some(owners) = self.demands.get_mut(plugin) else {
            return DemandRelease::NotHeld;
        };
        if !owners.remove(client) {
            return DemandRelease::NotHeld;
        }
        if owners.is_empty() {
            self.demands.remove(plugin);
            DemandRelease::NoLongerWanted
        } else {
            DemandRelease::StillWanted
        }
    }

    /// Drops every demand held by `client`, returning the plugins that are now
    /// unowned.
    ///
    /// Used when a client goes away: it must not keep a service alive, but it must
    /// also not stop one another client is still waiting on.
    pub fn release_client(&mut self, client: &str) -> Vec<String> {
        let mut unowned = Vec::new();
        for (plugin, owners) in self.demands.iter_mut() {
            if owners.remove(client) && owners.is_empty() {
                unowned.push(plugin.clone());
            }
        }
        for plugin in &unowned {
            self.demands.remove(plugin);
        }
        unowned
    }

    /// Every client currently waiting on `plugin`, in a stable order.
    pub fn owners(&self, plugin: &str) -> Vec<ClientRunId> {
        self.demands
            .get(plugin)
            .map(|owners| owners.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether anybody is waiting on `plugin`.
    pub fn is_wanted(&self, plugin: &str) -> bool {
        self.demands
            .get(plugin)
            .is_some_and(|owners| !owners.is_empty())
    }

    /// Plugins that currently have at least one owner.
    pub fn wanted_plugins(&self) -> Vec<String> {
        self.demands.keys().cloned().collect()
    }

    /// Chooses the client a plugin's work is derived for, or explains why it runs
    /// for nobody.
    ///
    /// The rule is deliberately strict. A derivation without an explicit source is
    /// not attributed to a guess: if two clients want the plugin and the trigger
    /// did not say which one caused it, inventing an answer would hand one client's
    /// work to another -- and the client that actually asked would never see its
    /// result. So multiple candidates without a source is [`Derivation::Ambiguous`],
    /// which the caller must treat as "do not run".
    ///
    /// A single owner needs no source: there is exactly one possible answer, and
    /// requiring the trigger to restate it would only add a way to get it wrong.
    pub fn derivation_owner(&self, plugin: &str, source: Option<&str>) -> Derivation {
        let owners = self.owners(plugin);
        if owners.is_empty() {
            // Configuration may allow the plugin, but nobody is waiting. Starting
            // it here is how a background service outlives its reason to exist.
            return Derivation::NoOwner;
        }
        if owners.len() == 1 {
            return Derivation::Owner(owners[0].clone());
        }
        match source {
            // The source must be one of the waiters. A source that holds no demand
            // is not evidence of anything about this plugin's work.
            Some(source) if owners.iter().any(|owner| owner == source) => {
                Derivation::Owner(source.to_string())
            }
            Some(_) | None => Derivation::Ambiguous { candidates: owners },
        }
    }
}

/// Who a plugin-triggered derivation runs for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation {
    /// Exactly one client, named explicitly.
    Owner(ClientRunId),
    /// Nobody is waiting: the derivation must not run.
    NoOwner,
    /// Several clients wait and the trigger did not name a valid source.
    Ambiguous { candidates: Vec<ClientRunId> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_does_not_stop_a_service_another_client_is_using() {
        let mut table = PluginDemandTable::new();
        assert!(table.acquire("app", "search"), "first acquire starts it");
        assert!(!table.acquire("tui", "search"), "second acquire reuses it");

        assert_eq!(table.release("app", "search"), DemandRelease::StillWanted);
        assert!(table.is_wanted("search"), "tui is still waiting on it");
        // Sorted, not insertion order: `owners` promises a stable answer so a
        // caller picking "the" owner does not see it reshuffle run to run.
        assert_eq!(table.owners("search"), vec!["tui".to_string()]);

        assert_eq!(
            table.release("tui", "search"),
            DemandRelease::NoLongerWanted
        );
        assert!(!table.is_wanted("search"));
    }

    #[test]
    fn acquiring_again_after_the_last_release_starts_it_again() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        assert_eq!(
            table.release("app", "search"),
            DemandRelease::NoLongerWanted
        );
        assert!(
            table.acquire("app", "search"),
            "the service must be startable again after everybody left"
        );
    }

    #[test]
    fn an_acquire_is_idempotent_so_a_retry_cannot_double_count() {
        let mut table = PluginDemandTable::new();
        assert!(table.acquire("app", "search"));
        assert!(!table.acquire("app", "search"));
        // One release is enough: counting twice would need two releases, and a
        // caller that retried only once would leak the service.
        assert_eq!(
            table.release("app", "search"),
            DemandRelease::NoLongerWanted
        );
    }

    #[test]
    fn an_unbalanced_release_cannot_stop_a_service() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        assert_eq!(
            table.release("tui", "search"),
            DemandRelease::NotHeld,
            "tui never asked for it"
        );
        assert!(table.is_wanted("search"), "app still needs it");
        assert_eq!(
            table.release("app", "unknown-plugin"),
            DemandRelease::NotHeld
        );
    }

    #[test]
    fn a_departing_client_only_stops_the_plugins_nobody_else_uses() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        table.acquire("app", "lsp");
        table.acquire("tui", "search");

        let mut unowned = table.release_client("app");
        unowned.sort();
        assert_eq!(unowned, vec!["lsp".to_string()], "tui still uses search");
        assert!(table.is_wanted("search"));
        assert!(!table.is_wanted("lsp"));
        assert_eq!(table.owners("lsp"), Vec::<String>::new());
    }

    #[test]
    fn owners_are_reported_in_a_stable_order() {
        let mut table = PluginDemandTable::new();
        table.acquire("tui", "search");
        table.acquire("app", "search");
        table.acquire("headless", "search");
        assert_eq!(
            table.owners("search"),
            vec!["app".to_string(), "headless".to_string(), "tui".to_string()]
        );
        assert_eq!(table.wanted_plugins(), vec!["search".to_string()]);
    }
}

#[cfg(test)]
mod derivation_tests {
    use super::*;

    #[test]
    fn a_derivation_without_an_owner_does_not_run() {
        // Enabled in configuration is not demand: nothing is waiting, so there is
        // nobody whose work this could be.
        let table = PluginDemandTable::new();
        assert_eq!(table.derivation_owner("search", None), Derivation::NoOwner);
        assert_eq!(
            table.derivation_owner("search", Some("app")),
            Derivation::NoOwner
        );
    }

    #[test]
    fn a_single_owner_needs_no_source() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        assert_eq!(
            table.derivation_owner("search", None),
            Derivation::Owner("app".to_string())
        );
        // A stale source that names nobody does not change the only possible answer.
        assert_eq!(
            table.derivation_owner("search", Some("departed")),
            Derivation::Owner("app".to_string())
        );
    }

    #[test]
    fn several_owners_without_a_source_are_ambiguous_not_a_guess() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        table.acquire("tui", "search");
        let decision = table.derivation_owner("search", None);
        assert_eq!(
            decision,
            Derivation::Ambiguous {
                candidates: vec!["app".to_string(), "tui".to_string()]
            },
            "picking one at random would hand one client's work to another"
        );
    }

    #[test]
    fn an_explicit_source_selects_its_own_work() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        table.acquire("tui", "search");
        assert_eq!(
            table.derivation_owner("search", Some("tui")),
            Derivation::Owner("tui".to_string())
        );
    }

    #[test]
    fn a_source_that_holds_no_demand_is_not_evidence() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        table.acquire("tui", "search");
        // `headless` never asked for this plugin, so claiming it caused the trigger
        // is not a reason to route the work to it.
        assert!(matches!(
            table.derivation_owner("search", Some("headless")),
            Derivation::Ambiguous { .. }
        ));
    }

    /// A plugin that stopped because everybody left must be startable again.
    ///
    /// The tempting bug is to remember "this plugin was stopped" and skip the next
    /// acquire: the client that comes back then waits forever for a service nobody
    /// starts. Demand is about who is waiting *now*.
    #[test]
    fn plugin_demand_reacquires_after_client_return() {
        let mut table = PluginDemandTable::new();
        assert!(table.acquire("app", "search"), "the first client starts it");
        assert_eq!(
            table.release("app", "search"),
            DemandRelease::NoLongerWanted
        );
        assert!(!table.is_wanted("search"));
        assert_eq!(table.derivation_owner("search", None), Derivation::NoOwner);

        assert!(
            table.acquire("app", "search"),
            "a returning client must start the service again"
        );
        assert_eq!(
            table.derivation_owner("search", None),
            Derivation::Owner("app".to_string())
        );
    }

    #[test]
    fn a_departed_client_leaves_the_remaining_owner_in_charge() {
        let mut table = PluginDemandTable::new();
        table.acquire("app", "search");
        table.acquire("tui", "search");
        table.release("app", "search");
        // A's departure does not hand its work to B: the derivation now has exactly
        // one possible owner, and it is the one still waiting.
        assert_eq!(
            table.derivation_owner("search", None),
            Derivation::Owner("tui".to_string())
        );
    }
}
