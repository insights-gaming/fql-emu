//! Lexical environment for `Let` bindings and lambda parameters.
//!
//! A persistent linked list so that lambda closures can capture it cheaply.

use crate::value::Value;
use std::rc::Rc;

#[derive(Clone, Default)]
pub struct Env(Option<Rc<Node>>);

struct Node {
    name: String,
    value: Value,
    parent: Env,
}

impl Env {
    pub fn empty() -> Env {
        Env(None)
    }

    pub fn bind(&self, name: &str, value: Value) -> Env {
        Env(Some(Rc::new(Node { name: name.to_string(), value, parent: self.clone() })))
    }

    pub fn lookup(&self, name: &str) -> Option<&Value> {
        let mut cur = &self.0;
        while let Some(node) = cur {
            if node.name == name {
                return Some(&node.value);
            }
            cur = &node.parent.0;
        }
        None
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names = Vec::new();
        let mut cur = &self.0;
        while let Some(node) = cur {
            names.push(node.name.as_str());
            cur = &node.parent.0;
        }
        write!(f, "Env{:?}", names)
    }
}
