//! Owner rules for the privacy filter: name-based, so a folder
//! created after the rules were set (a new account's profile, a recycle
//! bin on a new drive) is covered the moment it is indexed.
//!
//! A rule names a folder by path (`["C:", "Users"]`; `"*:"` matches any
//! drive root) and gives each child folder of it an owner: the one listed
//! for its name, else the rule's default. Names compare case-insensitively
//! (Unicode lowercase), never accent-folded: `BÖB` is not `bob`.

use crate::index::{OWNER_NONE, Owner};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRule {
    pub path: Vec<String>,
    pub children: Vec<(String, Owner)>,
    pub default: Owner,
}

/// Wildcard for a drive root in [`OwnerRule::path`].
pub const ANY_DRIVE: &str = "*:";

#[derive(Default, Clone, Debug)]
struct Node {
    next: HashMap<String, u32>,
    /// (child name -> owner, default) when a rule sits on this folder.
    rule: Option<(HashMap<String, Owner>, Owner)>,
}

/// Compiled rules: a trie over lowercase folder names from the drive
/// root. Node 0 is above the drive roots.
#[derive(Default, Clone, Debug)]
pub struct OwnerRules {
    nodes: Vec<Node>,
    any_drive: Option<u32>,
}

/// Node id meaning "no rule below here".
pub const NO_NODE: u32 = u32::MAX;

fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// Of two defaults for the same folder, the one that shows less.
fn stricter(a: Owner, b: Owner) -> Owner {
    if a == OWNER_NONE { b } else { a }
}

impl OwnerRules {
    pub fn new(rules: &[OwnerRule]) -> OwnerRules {
        let mut r = OwnerRules {
            nodes: vec![Node::default()],
            any_drive: None,
        };
        if rules.is_empty() {
            return r;
        }
        // an exact-drive subtree must also carry every any-drive rule, so
        // copy those under each named drive before building
        let drives: Vec<String> = rules
            .iter()
            .filter_map(|x| x.path.first())
            .filter(|d| d.as_str() != ANY_DRIVE)
            .map(|d| lower(d))
            .collect();
        let mut all: Vec<OwnerRule> = rules.to_vec();
        for w in rules
            .iter()
            .filter(|x| x.path.first().is_some_and(|d| d == ANY_DRIVE))
        {
            for d in &drives {
                let mut c = w.clone();
                c.path[0] = d.clone();
                all.push(c);
            }
        }
        for rule in &all {
            let Some((first, rest)) = rule.path.split_first() else {
                continue;
            };
            let mut n = if first == ANY_DRIVE {
                match r.any_drive {
                    Some(n) => n,
                    None => {
                        let n = r.push();
                        r.any_drive = Some(n);
                        n
                    }
                }
            } else {
                r.step_or_add(0, &lower(first))
            };
            for part in rest {
                n = r.step_or_add(n, &lower(part));
            }
            let node = &mut r.nodes[n as usize];
            let (children, default) = node
                .rule
                .get_or_insert_with(|| (HashMap::new(), OWNER_NONE));
            *default = stricter(*default, rule.default);
            for (name, o) in &rule.children {
                // an exact rule was copied before the any-drive ones run
                // into it here, so the first listing of a name wins
                children.entry(lower(name)).or_insert(*o);
            }
        }
        r
    }

    fn push(&mut self) -> u32 {
        self.nodes.push(Node::default());
        (self.nodes.len() - 1) as u32
    }

    fn step_or_add(&mut self, n: u32, name: &str) -> u32 {
        if let Some(&c) = self.nodes[n as usize].next.get(name) {
            return c;
        }
        let c = self.push();
        self.nodes[n as usize].next.insert(name.to_string(), c);
        c
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    /// For a folder named `name` under a folder at trie node `parent`
    /// (`None` = a drive root): its own trie node, and the owner a rule on
    /// the parent gives it (`None` = no rule applies).
    pub fn child(&self, parent: Option<u32>, name: &str) -> (u32, Option<Owner>) {
        if self.is_empty() {
            return (NO_NODE, None);
        }
        match parent {
            None => {
                let l = lower(name);
                let n = self.nodes[0]
                    .next
                    .get(&l)
                    .copied()
                    .or(self.any_drive)
                    .unwrap_or(NO_NODE);
                (n, None)
            }
            Some(NO_NODE) => (NO_NODE, None),
            Some(p) => {
                let l = lower(name);
                let node = &self.nodes[p as usize];
                let owner = node
                    .rule
                    .as_ref()
                    .map(|(c, d)| c.get(&l).copied().unwrap_or(*d));
                (node.next.get(&l).copied().unwrap_or(NO_NODE), owner)
            }
        }
    }
}
