//! Scope hierarchy: a lodge grant covers the patrols declared inside it.
//!
//! The core does not model a lodge→patrol hierarchy itself and never calls a
//! plugin during authorization. Instead, plugins **declare edges** into
//! `core.scope_hierarchy` and the core loads them into this in-memory map at
//! boot and on reload. A caller's grants are then expanded with the declared
//! descendants of each grant's scope, bounded in depth, so the (flat, unchanged)
//! SDK `Scope::covers` resolves the hierarchy for both the route gate and
//! in-handler checks — no plugin is consulted and no SDK/ABI change is needed.
//!
//! Semantics:
//! - `Troop` covers everything (unchanged).
//! - A grant at scope `S` covers target `T` if `S == T`, or `T` is a descendant
//!   of `S` through the declared edges. Direction is **downward only**: a patrol
//!   grant never covers its lodge, and a child never widens a parent.
//! - No edge → no coverage. Cycles/self-edges are rejected; the walk is bounded.
//!
//! See `docs/design/scoped-permissions.md` §3.2 and §8 decision 5.

use std::collections::{HashMap, HashSet, VecDeque};

use adjutant_sdk::{Identity, RoleGrant, Scope, ScopeType};

type Node = (String, String);

/// Maximum edges followed from a grant scope to a target. The hierarchy the
/// Accords describe is two levels (lodge → patrol); three is comfortable headroom.
pub const MAX_DEPTH: usize = 3;

/// Declared scope edges, parent → children. Cycles are removed at construction.
#[derive(Debug, Default, Clone)]
pub struct ScopeHierarchy {
    children: HashMap<Node, Vec<Node>>,
}

impl ScopeHierarchy {
    /// Build from raw `(parent_type, parent_id, child_type, child_id)` rows.
    /// Self-edges and edges that would close a cycle are dropped with a warning,
    /// so a bad declaration can neither hang the walk nor silently widen a scope.
    pub fn from_edges<I: IntoIterator<Item = (String, String, String, String)>>(edges: I) -> Self {
        let mut children: HashMap<Node, Vec<Node>> = HashMap::new();
        for (pt, pid, ct, cid) in edges {
            // Only the scope types the SDK can represent (troop/lodge/patrol);
            // troop is the implicit root and carries no id, so it is never a
            // declared endpoint. Unknown types are refused, not mis-mapped.
            if !matches!(pt.as_str(), "lodge" | "patrol") || !matches!(ct.as_str(), "lodge" | "patrol") {
                tracing::warn!(parent_type = %pt, child_type = %ct,
                    "scope hierarchy: refusing an edge with an unrepresentable scope type");
                continue;
            }
            let parent = (pt.clone(), pid.clone());
            let child = (ct.clone(), cid.clone());
            if parent == child {
                tracing::warn!(?parent, "scope hierarchy: refusing a self-edge");
                continue;
            }
            // Adding parent→child closes a cycle iff `parent` is already
            // reachable from `child`.
            if reachable(&children, &child, &parent) {
                tracing::warn!(?parent, ?child, "scope hierarchy: refusing an edge that closes a cycle");
                continue;
            }
            children.entry(parent).or_default().push(child);
        }
        Self { children }
    }

    /// Load the declared edges from `core.scope_hierarchy`.
    pub async fn load(pool: &sqlx::PgPool) -> Result<Self, sqlx::Error> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT parent_type, parent_id, child_type, child_id FROM core.scope_hierarchy",
        )
        .fetch_all(pool)
        .await?;
        Ok(Self::from_edges(rows))
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Descendants of `scope` within [`MAX_DEPTH`], excluding `scope` itself. A
    /// troop scope has no id and needs no expansion (it already covers all).
    fn descendants(&self, scope: &Scope) -> Vec<Scope> {
        let Some(id) = scope.scope_id.as_ref() else {
            return Vec::new();
        };
        let start = (type_name(scope.scope_type).to_string(), id.clone());
        let mut seen: HashSet<Node> = HashSet::new();
        seen.insert(start.clone());
        let mut queue: VecDeque<(Node, usize)> = VecDeque::from([(start, 0)]);
        let mut out = Vec::new();
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= MAX_DEPTH {
                continue;
            }
            if let Some(kids) = self.children.get(&node) {
                for kid in kids {
                    if seen.insert(kid.clone()) {
                        out.push(scope_from_node(kid));
                        queue.push_back((kid.clone(), depth + 1));
                    }
                }
            }
        }
        out
    }

    /// The caller's grants, each expanded with its declared descendants. The
    /// result has the same roles, so `Identity::roles()` is unchanged.
    pub fn expand(&self, identity: &Identity) -> Identity {
        let mut grants = identity.grants.clone();
        let mut seen: HashSet<(String, String, String)> = grants
            .iter()
            .map(|g| (g.role_id.clone(), type_name(g.scope.scope_type).into(), g.scope.scope_id.clone().unwrap_or_default()))
            .collect();
        for g in &identity.grants {
            for descendant in self.descendants(&g.scope) {
                let key = (
                    g.role_id.clone(),
                    type_name(descendant.scope_type).into(),
                    descendant.scope_id.clone().unwrap_or_default(),
                );
                if seen.insert(key) {
                    grants.push(RoleGrant { role_id: g.role_id.clone(), scope: descendant });
                }
            }
        }
        Identity::from_grants(identity.user_id.clone(), grants)
    }
}

/// Is `target` reachable from `from` by following declared edges (bounded)?
fn reachable(children: &HashMap<Node, Vec<Node>>, from: &Node, target: &Node) -> bool {
    let mut seen: HashSet<&Node> = HashSet::new();
    let mut queue: VecDeque<(&Node, usize)> = VecDeque::from([(from, 0)]);
    while let Some((node, depth)) = queue.pop_front() {
        if node == target {
            return true;
        }
        if depth >= MAX_DEPTH || !seen.insert(node) {
            continue;
        }
        if let Some(kids) = children.get(node) {
            for kid in kids {
                queue.push_back((kid, depth + 1));
            }
        }
    }
    false
}

fn type_name(t: ScopeType) -> &'static str {
    match t {
        ScopeType::Troop => "troop",
        ScopeType::Lodge => "lodge",
        ScopeType::Patrol => "patrol",
    }
}

fn scope_from_node(node: &Node) -> Scope {
    let scope_type = if node.0 == "lodge" {
        ScopeType::Lodge
    } else {
        ScopeType::Patrol
    };
    Scope { scope_type, scope_id: Some(node.1.clone()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(pt: &str, pid: &str, ct: &str, cid: &str) -> (String, String, String, String) {
        (pt.into(), pid.into(), ct.into(), cid.into())
    }

    fn lodge_id_covered(h: &ScopeHierarchy, grant: &Scope, target: &Scope) -> bool {
        let id = Identity::from_grants(
            "u",
            vec![RoleGrant { role_id: "r".into(), scope: grant.clone() }],
        );
        h.expand(&id).roles_covering(target).contains(&"r".to_string())
    }

    #[test]
    fn lodge_grant_covers_a_declared_patrol() {
        let h = ScopeHierarchy::from_edges([edge("lodge", "3", "patrol", "7")]);
        assert!(lodge_id_covered(&h, &Scope::lodge("3"), &Scope::patrol("7")));
        assert!(!lodge_id_covered(&h, &Scope::lodge("3"), &Scope::patrol("8")));
        // no edge for lodge 4 → patrol 8 is denied
        assert!(!lodge_id_covered(&h, &Scope::lodge("4"), &Scope::patrol("8")));
    }

    #[test]
    fn covering_is_downward_only() {
        let h = ScopeHierarchy::from_edges([edge("lodge", "3", "patrol", "7")]);
        // A patrol grant must not cover its own lodge.
        assert!(!lodge_id_covered(&h, &Scope::patrol("7"), &Scope::lodge("3")));
        // Nor an unrelated patrol.
        assert!(!lodge_id_covered(&h, &Scope::patrol("7"), &Scope::patrol("8")));
    }

    #[test]
    fn troop_covers_everything() {
        let h = ScopeHierarchy::from_edges([]);
        assert!(lodge_id_covered(&h, &Scope::troop(), &Scope::patrol("7")));
        assert!(lodge_id_covered(&h, &Scope::troop(), &Scope::lodge("3")));
        assert!(lodge_id_covered(&h, &Scope::troop(), &Scope::troop()));
    }

    #[test]
    fn the_walk_is_depth_bounded_and_terminates() {
        // The SDK's `ScopeType` has only troop/lodge/patrol, so a real chain is
        // one level (lodge→patrol). This synthetic alternating chain proves the
        // depth bound and that the walk terminates:
        // lodge1 -> patrol2 -> lodge3 -> patrol4 -> lodge5.
        let h = ScopeHierarchy::from_edges([
            edge("lodge", "1", "patrol", "2"),
            edge("patrol", "2", "lodge", "3"),
            edge("lodge", "3", "patrol", "4"),
            edge("patrol", "4", "lodge", "5"),
        ]);
        let id = Identity::from_grants(
            "u",
            vec![RoleGrant { role_id: "r".into(), scope: Scope::lodge("1") }],
        );
        let expanded = h.expand(&id);
        // lodge1 grant expands to patrol2 (d1), lodge3 (d2), patrol4 (d3).
        assert!(expanded.grants.iter().any(|g| g.scope == Scope::patrol("4")));
        // lodge5 would be depth 4, beyond MAX_DEPTH, so it is not included.
        assert!(
            !expanded.grants.iter().any(|g| g.scope == Scope::lodge("5")),
            "the walk must be bounded at MAX_DEPTH"
        );
        assert!(expanded.grants.len() <= id.grants.len() + MAX_DEPTH);
    }

    #[test]
    fn unrepresentable_scope_types_are_refused() {
        // `squad` has no `ScopeType`; the edge must be dropped, not mis-mapped.
        let h = ScopeHierarchy::from_edges([edge("lodge", "3", "squad", "9")]);
        assert!(h.is_empty());
    }

    #[test]
    fn self_edges_and_cycles_are_rejected() {
        // A self-edge is dropped.
        let h = ScopeHierarchy::from_edges([edge("patrol", "7", "patrol", "7")]);
        assert!(h.is_empty());

        // A two-node cycle: the second edge is dropped; the first still works,
        // and neither direction allows an upward/widening match beyond the edge.
        let h = ScopeHierarchy::from_edges([
            edge("lodge", "3", "patrol", "7"),
            edge("patrol", "7", "lodge", "3"),
        ]);
        assert!(lodge_id_covered(&h, &Scope::lodge("3"), &Scope::patrol("7")));
        assert!(
            !lodge_id_covered(&h, &Scope::patrol("7"), &Scope::lodge("3")),
            "the cycle-closing edge must be rejected, so a patrol cannot cover a lodge"
        );
    }

    #[test]
    fn expansion_does_not_duplicate_or_change_roles() {
        let h = ScopeHierarchy::from_edges([
            edge("lodge", "3", "patrol", "7"),
            edge("lodge", "3", "patrol", "8"),
        ]);
        let id = Identity::from_grants(
            "u",
            vec![RoleGrant { role_id: "r".into(), scope: Scope::lodge("3") }],
        );
        let expanded = h.expand(&id);
        assert_eq!(expanded.roles(), vec!["r".to_string()]);
        assert_eq!(expanded.grants.len(), 3, "lodge + two patrol grants");
    }
}
