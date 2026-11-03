//! Phase lanes: an agent with many helpers shows them grouped by what they
//! do — investigate, build, check, ship — each group a lane of small cards
//! stacked under a header, instead of one long row of full cards.
//!
//! The phase is read off the helper's role as its format names it (Codex's
//! `agent_role`), else its agent type (Claude's `Explore`, `code-reviewer`).
//! A role that names no phase goes to the last lane, `other`.

use super::session::SessionModel;

/// An agent needs at least this many helpers before they go into lanes;
/// fewer sit side by side as before.
pub const LANE_MIN: usize = 5;

/// What a helper is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    Investigate,
    Build,
    Check,
    Ship,
    Other,
}

impl Phase {
    /// The lane's header.
    pub fn title(self) -> &'static str {
        match self {
            Phase::Investigate => "INVESTIGATE",
            Phase::Build => "BUILD",
            Phase::Check => "CHECK",
            Phase::Ship => "SHIP",
            Phase::Other => "OTHER",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Phase::Investigate => "investigate",
            Phase::Build => "build",
            Phase::Check => "check",
            Phase::Ship => "ship",
            Phase::Other => "other",
        }
    }

    /// The phase a role names, by its words: `lint_checker` is a check,
    /// `implementer` a build. Checking wins over building, so a
    /// `test_writer` checks.
    pub fn of(role: &str) -> Phase {
        let role = role.to_lowercase();
        let words: Vec<&str> = role
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let any = |keys: &[&str]| words.iter().any(|w| keys.iter().any(|k| w.starts_with(k)));
        if any(&[
            "test", "check", "lint", "review", "verif", "qa", "audit", "valid", "inspect",
        ]) {
            Phase::Check
        } else if any(&[
            "research",
            "explor",
            "architect",
            "plan",
            "investig",
            "analy",
            "scout",
            "search",
            "design",
        ]) {
            Phase::Investigate
        } else if any(&[
            "implement",
            "fix",
            "build",
            "cod",
            "writ",
            "refactor",
            "edit",
            "develop",
            "engineer",
        ]) {
            Phase::Build
        } else if any(&["publish", "ship", "release", "deploy", "merge", "commit"])
            || words.contains(&"pr")
        {
            Phase::Ship
        } else {
            Phase::Other
        }
    }
}

/// One lane: an agent's helpers of one phase, in spawn order.
#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    /// The agent the helpers belong to (model id).
    pub parent: String,
    pub phase: Phase,
    /// The helpers (model ids).
    pub kids: Vec<String>,
}

impl Lane {
    /// The lane header's node id, under the session's id prefix. Plane
    /// structure, like a zone: `◈`-led, so selection steps over it.
    pub fn node_id(&self, ns: &str) -> String {
        format!("◈lane/{ns}{}/{}", self.parent, self.phase.slug())
    }
}

/// The lanes of every agent in `model` with [`LANE_MIN`] or more helpers,
/// each parent's in phase order.
pub fn lanes(model: &SessionModel) -> Vec<Lane> {
    let mut kids: Vec<(String, Vec<String>)> = Vec::new();
    for id in model.spawn_order() {
        let Some(parent) = model.agent(id).and_then(|a| a.parent.clone()) else {
            continue;
        };
        if model.agent(&parent).is_none() {
            continue;
        }
        match kids.iter_mut().find(|(p, _)| *p == parent) {
            Some((_, list)) => list.push(id.to_string()),
            None => kids.push((parent, vec![id.to_string()])),
        }
    }
    let mut out = Vec::new();
    for (parent, list) in kids.into_iter().filter(|(_, l)| l.len() >= LANE_MIN) {
        let mut by: Vec<(Phase, Vec<String>)> = Vec::new();
        for id in list {
            let phase = model
                .agent(&id)
                .and_then(|a| a.role.as_deref().or(a.agent_type.as_deref()))
                .map_or(Phase::Other, Phase::of);
            match by.iter_mut().find(|(p, _)| *p == phase) {
                Some((_, l)) => l.push(id),
                None => by.push((phase, vec![id])),
            }
        }
        by.sort_by_key(|(p, _)| *p);
        out.extend(by.into_iter().map(|(phase, kids)| Lane {
            parent: parent.clone(),
            phase,
            kids,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_name_their_phase() {
        let cases = [
            ("researcher", Phase::Investigate),
            ("architect", Phase::Investigate),
            ("Explore", Phase::Investigate),
            ("implementer", Phase::Build),
            ("fixer", Phase::Build),
            ("test_runner", Phase::Check),
            ("database_checker", Phase::Check),
            ("lint_checker", Phase::Check),
            ("code-reviewer", Phase::Check),
            ("test_writer", Phase::Check),
            ("publisher", Phase::Ship),
            ("open_pr", Phase::Ship),
            ("default", Phase::Other),
            ("general-purpose", Phase::Other),
            ("printer", Phase::Other),
        ];
        for (role, phase) in cases {
            assert_eq!(Phase::of(role), phase, "{role}");
        }
    }
}
