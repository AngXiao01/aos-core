use alloc::string::String;
use alloc::vec::Vec;

use crate::predicate::Predicate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderDir {
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderKey {
    pub field: String,
    pub dir: OrderDir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Aggregate {
    pub count: bool,
    pub histogram: Option<String>,
    pub mean: Option<String>,
}

impl Default for Aggregate {
    fn default() -> Self {
        Self {
            count: false,
            histogram: None,
            mean: None,
        }
    }
}

/// Structured query. JSON parsing lives in `aos-query`; this is the shared AST
/// also used by preconditions' sibling types.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub from: String,
    pub filter: Option<Predicate>,
    pub select: Vec<String>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<u64>,
    pub cursor: Option<String>,
    pub aggregate: Aggregate,
}

impl Query {
    pub fn of_type(from: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            filter: None,
            select: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            cursor: None,
            aggregate: Aggregate::default(),
        }
    }

    pub fn effective_limit(&self) -> usize {
        let n = self.limit.unwrap_or(50);
        n.min(1000) as usize
    }
}
