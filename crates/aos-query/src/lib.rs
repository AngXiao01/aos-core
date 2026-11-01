//! Query language from spec §13.1: filter, page, cursor, aggregates.
//!
//! The AST lives in `aos-types` so preconditions and this executor share it.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use aos_types::{
    cmp_numbers, field_of, hex_decode, hex_encode, sha256, Aggregate, Cbor, CmpOp, ContentHash,
    Object, ObjectId, ObjectView, OrderDir, OrderKey, Predicate, Query, RefEdge, Value,
};

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub rows: Vec<BTreeMap<String, Value>>,
    pub next_cursor: Option<String>,
    /// Count of matches before pagination, when requested.
    pub count: Option<u64>,
    pub histogram: BTreeMap<String, u64>,
    pub mean: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    Json(usize),
    Shape,
    Cursor,
}

pub fn execute(view: &dyn ObjectView, query: &Query) -> Result<Page, QueryError> {
    execute_with(view, query, &mut |_| {})
}

/// Like [`execute`], and calls `on_match` once for every object that passed
/// the filter, before paging. Aggregates are computed over all of them, so a
/// host that tracks what a guest has seen (taint lineage) needs the full set,
/// not just the returned rows.
pub fn execute_with(
    view: &dyn ObjectView,
    query: &Query,
    on_match: &mut dyn FnMut(&Object),
) -> Result<Page, QueryError> {
    let mut matched: Vec<Object> = Vec::new();
    if let Some(target) = ref_seed(query.filter.as_ref()) {
        // The reverse index only proposes candidates; `passes` re-checks the
        // full predicate. Edge field names are not compared here because the
        // filter may spell the path as `x`, `fields.x`, or reach `extra.x`
        // through `field_of`'s fallback.
        let mut seen = BTreeSet::new();
        for edge in view.reverse(&target) {
            if !seen.insert(edge.source) {
                continue;
            }
            if let Some(obj) = view.get(&edge.source) {
                if obj.type_name == query.from && !obj.tombstone && passes(&obj, query) {
                    matched.push(obj);
                }
            }
        }
    } else {
        view.for_each(&mut |obj| {
            if obj.type_name == query.from && !obj.tombstone && passes(obj, query) {
                matched.push(obj.clone());
            }
        });
    }
    // Validate the cursor before reporting matches, so a bad request reveals nothing.
    let cursor = match &query.cursor {
        Some(cur) => Some(decode_cursor(cur)?),
        None => None,
    };
    matched.sort_by(|a, b| cmp_objects(a, b, &query.order_by));
    for obj in &matched {
        on_match(obj);
    }
    let count = matched.len() as u64;
    let histogram = if let Some(field) = &query.aggregate.histogram {
        let mut h = BTreeMap::new();
        for obj in &matched {
            let key = match field_of(obj, field) {
                Some(v) => display_value(&v),
                None => String::new(),
            };
            *h.entry(key).or_insert(0) += 1;
        }
        h
    } else {
        BTreeMap::new()
    };
    let mean = if let Some(field) = &query.aggregate.mean {
        let mut sum = 0.0f64;
        let mut n = 0u64;
        for obj in &matched {
            if let Some(v) = field_of(obj, field) {
                if let Some(x) = as_num(&v) {
                    sum += x;
                    n += 1;
                }
            }
        }
        if n == 0 {
            None
        } else {
            Some(sum / n as f64)
        }
    } else {
        None
    };
    if let Some(key) = &cursor {
        matched.retain(|obj| cmp_cursor(obj, &query.order_by, key) > 0);
    }
    let limit = query.effective_limit();
    let mut next_cursor = None;
    if matched.len() > limit {
        // `limit: 0` asks for aggregates only; there is no last row to resume after.
        if let Some(last) = limit.checked_sub(1).map(|i| &matched[i]) {
            next_cursor = Some(encode_cursor(last, &query.order_by));
        }
        matched.truncate(limit);
    }
    let rows = matched
        .into_iter()
        .map(|obj| project(&obj, &query.select))
        .collect();
    Ok(Page {
        rows,
        next_cursor,
        count: if query.aggregate.count {
            Some(count)
        } else {
            None
        },
        histogram,
        mean,
    })
}

fn passes(obj: &Object, query: &Query) -> bool {
    match &query.filter {
        None => true,
        Some(p) => aos_types::eval_predicate(p, &mut |path| field_of(obj, path)),
    }
}

/// A top-level equality on a ref, or one inside `all`, can use the reverse index.
fn ref_seed(pred: Option<&Predicate>) -> Option<ObjectId> {
    let pred = pred?;
    match pred {
        Predicate::Cmp {
            op: CmpOp::Eq,
            value: Value::Ref(id) | Value::RefAt(id, _),
            ..
        } => Some(*id),
        Predicate::All(xs) => {
            for p in xs {
                if let Some(s) = ref_seed(Some(p)) {
                    return Some(s);
                }
            }
            None
        }
        _ => None,
    }
}

fn cmp_objects(a: &Object, b: &Object, order: &[OrderKey]) -> core::cmp::Ordering {
    for key in order {
        let av = field_of(a, &key.field);
        let bv = field_of(b, &key.field);
        let ord = cmp_opt_value(av.as_ref(), bv.as_ref());
        let ord = match key.dir {
            OrderDir::Asc => ord,
            OrderDir::Desc => ord.reverse(),
        };
        if ord != core::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.id.cmp(&b.id)
}

fn cmp_opt_value(a: Option<&Value>, b: Option<&Value>) -> core::cmp::Ordering {
    match (a, b) {
        (None, None) => core::cmp::Ordering::Equal,
        (None, Some(_)) => core::cmp::Ordering::Greater,
        (Some(_), None) => core::cmp::Ordering::Less,
        (Some(a), Some(b)) => cmp_value(a, b),
    }
}

/// Sort rank of a value's kind. Values of different kinds never compare by
/// their display text: `10 > 9 > "10" == 10` is not an order, and the
/// standard sort may panic on a comparator that is not total.
fn kind_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 2,
        Value::Text(_) => 3,
        Value::Bytes(_) => 4,
        Value::Ref(_) | Value::RefAt(_, _) => 5,
        Value::List(_) => 6,
        Value::Map(_) => 7,
    }
}

/// Total order over values: kind first, then a per-kind order. Numbers compare
/// exactly across `Int` and `Float`; NaN sorts after every other number.
fn cmp_value(a: &Value, b: &Value) -> Ordering {
    let by_kind = kind_rank(a).cmp(&kind_rank(b));
    if by_kind != Ordering::Equal {
        return by_kind;
    }
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Ref(_) | Value::RefAt(_, _), Value::Ref(_) | Value::RefAt(_, _)) => {
            ref_key(a).cmp(&ref_key(b))
        }
        (Value::Int(_) | Value::Float(_), _) => {
            cmp_numbers(a, b).unwrap_or_else(|| is_nan(a).cmp(&is_nan(b)))
        }
        (Value::List(_) | Value::Map(_), _) => {
            let ea = a.canonical().unwrap_or_default();
            let eb = b.canonical().unwrap_or_default();
            ea.cmp(&eb)
        }
        _ => Ordering::Equal,
    }
}

fn ref_key(v: &Value) -> (ObjectId, Option<u64>) {
    match v {
        Value::Ref(id) => (*id, None),
        Value::RefAt(id, ver) => (*id, Some(*ver)),
        _ => (ObjectId::ZERO, None),
    }
}

fn is_nan(v: &Value) -> bool {
    matches!(v, Value::Float(n) if n.is_nan())
}

fn display_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => String::from(if *b { "true" } else { "false" }),
        Value::Int(n) => itoa(*n),
        Value::Float(n) => {
            // Stable enough for histogram keys in tests.
            let s = alloc::format!("{n}");
            s
        }
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => hex_encode(b),
        Value::Ref(id) | Value::RefAt(id, _) => id.to_text(),
        Value::List(_) => String::from("[]"),
        Value::Map(_) => String::from("{}"),
    }
}

fn itoa(mut n: i64) -> String {
    if n == 0 {
        return String::from("0");
    }
    let neg = n < 0;
    let mut buf = Vec::new();
    if neg {
        // i64::MIN
        if n == i64::MIN {
            return String::from("-9223372036854775808");
        }
        n = -n;
    }
    while n > 0 {
        buf.push(b'0' + (n % 10) as u8);
        n /= 10;
    }
    if neg {
        buf.push(b'-');
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(n) if n.is_finite() => Some(*n),
        _ => None,
    }
}

fn project(obj: &Object, select: &[String]) -> BTreeMap<String, Value> {
    let mut row = BTreeMap::new();
    row.insert("id".into(), Value::Text(obj.id.to_text()));
    if select.is_empty() {
        for (k, v) in &obj.fields {
            row.insert(k.clone(), v.clone());
        }
        row.insert(
            "provenance.taint".into(),
            Value::Text(obj.provenance.taint.as_str().into()),
        );
        return row;
    }
    for field in select {
        if let Some(v) = field_of(obj, field) {
            row.insert(field.clone(), v);
        }
    }
    row
}

struct CursorKey {
    /// `None` when the last row had no value for that sort key. A missing
    /// value sorts after every present one, so it must not be confused with
    /// an explicit `null`, which sorts first.
    values: Vec<Option<Value>>,
    id: ObjectId,
}

/// Each sort key is `[]` (missing) or `[value]` (present).
fn encode_cursor(obj: &Object, order: &[OrderKey]) -> String {
    let mut vals = Vec::new();
    for key in order {
        vals.push(match field_of(obj, &key.field) {
            Some(v) => match v.to_cbor() {
                Ok(c) => Cbor::Array(alloc::vec![c]),
                Err(_) => Cbor::Array(Vec::new()),
            },
            None => Cbor::Array(Vec::new()),
        });
    }
    let cbor = Cbor::Array(alloc::vec![
        Cbor::Array(vals),
        Cbor::Bytes(obj.id.0.to_vec()),
    ]);
    hex_encode(&cbor.canonical().unwrap_or_default())
}

fn decode_cursor(hex: &str) -> Result<CursorKey, QueryError> {
    let bytes = hex_decode(hex).map_err(|_| QueryError::Cursor)?;
    let cbor = aos_types::cbor::decode(&bytes).map_err(|_| QueryError::Cursor)?;
    let Cbor::Array(xs) = cbor else {
        return Err(QueryError::Cursor);
    };
    if xs.len() != 2 {
        return Err(QueryError::Cursor);
    }
    let Cbor::Array(vals) = &xs[0] else {
        return Err(QueryError::Cursor);
    };
    let Cbor::Bytes(idb) = &xs[1] else {
        return Err(QueryError::Cursor);
    };
    if idb.len() != 16 {
        return Err(QueryError::Cursor);
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(idb);
    let mut values = Vec::new();
    for v in vals {
        let Cbor::Array(slot) = v else {
            return Err(QueryError::Cursor);
        };
        values.push(match slot.as_slice() {
            [] => None,
            [present] => Some(Value::from_cbor(present).map_err(|_| QueryError::Cursor)?),
            _ => return Err(QueryError::Cursor),
        });
    }
    Ok(CursorKey {
        values,
        id: ObjectId(id),
    })
}

fn cmp_cursor(obj: &Object, order: &[OrderKey], key: &CursorKey) -> i8 {
    for (i, ord) in order.iter().enumerate() {
        let left = field_of(obj, &ord.field);
        let right = key.values.get(i).and_then(|v| v.as_ref());
        let cmp = cmp_opt_value(left.as_ref(), right);
        let cmp = match ord.dir {
            OrderDir::Asc => cmp,
            OrderDir::Desc => cmp.reverse(),
        };
        if cmp != core::cmp::Ordering::Equal {
            return if cmp == core::cmp::Ordering::Less {
                -1
            } else {
                1
            };
        }
    }
    match obj.id.cmp(&key.id) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Greater => 1,
        core::cmp::Ordering::Equal => 0,
    }
}

/// Arrays and objects nested deeper than this are rejected. Parsing recurses,
/// so an unbounded depth lets one request overflow the stack. The canonical
/// CBOR codec has the same ceiling, so nothing deeper could be stored anyway.
const MAX_JSON_DEPTH: usize = 64;

pub fn parse_query(json: &str) -> Result<Query, QueryError> {
    let mut p = Parser {
        bytes: json.as_bytes(),
        i: 0,
        depth: 0,
    };
    let v = p.parse_value()?;
    p.skip();
    if p.i != p.bytes.len() {
        return Err(QueryError::Json(p.i));
    }
    query_from_json(&v)
}

enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

struct Parser<'a> {
    bytes: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn parse_value(&mut self) -> Result<Json, QueryError> {
        self.skip();
        let b = *self.bytes.get(self.i).ok_or(QueryError::Json(self.i))?;
        match b {
            b'n' => self.consume(b"null").map(|_| Json::Null),
            b't' => self.consume(b"true").map(|_| Json::Bool(true)),
            b'f' => self.consume(b"false").map(|_| Json::Bool(false)),
            b'"' => Ok(Json::String(self.string()?)),
            b'[' | b'{' => {
                if self.depth >= MAX_JSON_DEPTH {
                    return Err(QueryError::Json(self.i));
                }
                self.depth += 1;
                let v = if b == b'[' {
                    self.array()
                } else {
                    self.object()
                };
                self.depth -= 1;
                v
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(QueryError::Json(self.i)),
        }
    }

    fn hex4(&mut self) -> Result<u32, QueryError> {
        let digits = self
            .bytes
            .get(self.i..self.i + 4)
            .ok_or(QueryError::Json(self.i))?;
        let mut n = 0u32;
        for &d in digits {
            let v = (d as char).to_digit(16).ok_or(QueryError::Json(self.i))?;
            n = (n << 4) | v;
        }
        self.i += 4;
        Ok(n)
    }

    /// `\uXXXX`, with UTF-16 surrogate pairs. Agents often send ASCII-only
    /// JSON (Python's default), so every non-ASCII character arrives this way.
    fn unicode_escape(&mut self, out: &mut Vec<u8>) -> Result<(), QueryError> {
        let at = self.i;
        let hi = self.hex4()?;
        let code = match hi {
            0xD800..=0xDBFF => {
                if self.bytes.get(self.i..self.i + 2) != Some(b"\\u") {
                    return Err(QueryError::Json(at));
                }
                self.i += 2;
                let lo = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(QueryError::Json(at));
                }
                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(QueryError::Json(at)),
            n => n,
        };
        let c = char::from_u32(code).ok_or(QueryError::Json(at))?;
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        Ok(())
    }

    fn consume(&mut self, lit: &[u8]) -> Result<(), QueryError> {
        if self.bytes.get(self.i..self.i + lit.len()) != Some(lit) {
            return Err(QueryError::Json(self.i));
        }
        self.i += lit.len();
        Ok(())
    }

    fn string(&mut self) -> Result<String, QueryError> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let b = *self.bytes.get(self.i).ok_or(QueryError::Json(self.i))?;
            self.i += 1;
            match b {
                b'"' => return Ok(String::from_utf8(out).map_err(|_| QueryError::Json(self.i))?),
                b'\\' => {
                    let e = *self.bytes.get(self.i).ok_or(QueryError::Json(self.i))?;
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'u' => self.unicode_escape(&mut out)?,
                        _ => return Err(QueryError::Json(self.i)),
                    }
                }
                _ => out.push(b),
            }
        }
    }

    fn number(&mut self) -> Result<Json, QueryError> {
        let start = self.i;
        if self.bytes.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while matches!(
            self.bytes.get(self.i),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.i += 1;
        }
        let s = core::str::from_utf8(&self.bytes[start..self.i])
            .map_err(|_| QueryError::Json(start))?;
        Ok(Json::Number(s.into()))
    }

    fn array(&mut self) -> Result<Json, QueryError> {
        self.i += 1;
        let mut xs = Vec::new();
        self.skip();
        if self.bytes.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Array(xs));
        }
        loop {
            xs.push(self.parse_value()?);
            self.skip();
            match self.bytes.get(self.i) {
                Some(&b',') => self.i += 1,
                Some(&b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(QueryError::Json(self.i)),
            }
        }
        Ok(Json::Array(xs))
    }

    fn object(&mut self) -> Result<Json, QueryError> {
        self.i += 1;
        let mut xs = Vec::new();
        self.skip();
        if self.bytes.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Object(xs));
        }
        loop {
            self.skip();
            if self.bytes.get(self.i) != Some(&b'"') {
                return Err(QueryError::Json(self.i));
            }
            let k = self.string()?;
            self.skip();
            if self.bytes.get(self.i) != Some(&b':') {
                return Err(QueryError::Json(self.i));
            }
            self.i += 1;
            let v = self.parse_value()?;
            xs.push((k, v));
            self.skip();
            match self.bytes.get(self.i) {
                Some(&b',') => self.i += 1,
                Some(&b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(QueryError::Json(self.i)),
            }
        }
        Ok(Json::Object(xs))
    }

    fn skip(&mut self) {
        while matches!(self.bytes.get(self.i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.i += 1;
        }
    }
}

fn query_from_json(v: &Json) -> Result<Query, QueryError> {
    let Json::Object(pairs) = v else {
        return Err(QueryError::Shape);
    };
    let mut q = Query::of_type("");
    for (k, val) in pairs {
        match k.as_str() {
            "from" => {
                let Json::String(s) = val else {
                    return Err(QueryError::Shape);
                };
                q.from = s.clone();
            }
            "where" => q.filter = Some(pred_from_json(val)?),
            "select" => q.select = string_list(val)?,
            "order_by" => q.order_by = order_from_json(val)?,
            "limit" => q.limit = Some(json_u64(val)?),
            "cursor" => match val {
                Json::Null => q.cursor = None,
                Json::String(s) => q.cursor = Some(s.clone()),
                _ => return Err(QueryError::Shape),
            },
            "aggregate" => q.aggregate = agg_from_json(val)?,
            _ => {}
        }
    }
    if q.from.is_empty() {
        return Err(QueryError::Shape);
    }
    Ok(q)
}

fn pred_from_json(v: &Json) -> Result<Predicate, QueryError> {
    let Json::Object(pairs) = v else {
        return Err(QueryError::Shape);
    };
    if let Some((_, inner)) = pairs.iter().find(|(k, _)| k == "all") {
        let Json::Array(xs) = inner else {
            return Err(QueryError::Shape);
        };
        let mut out = Vec::new();
        for x in xs {
            out.push(pred_from_json(x)?);
        }
        return Ok(Predicate::All(out));
    }
    if let Some((_, inner)) = pairs.iter().find(|(k, _)| k == "any") {
        let Json::Array(xs) = inner else {
            return Err(QueryError::Shape);
        };
        let mut out = Vec::new();
        for x in xs {
            out.push(pred_from_json(x)?);
        }
        return Ok(Predicate::Any(out));
    }
    if let Some((_, inner)) = pairs.iter().find(|(k, _)| k == "not") {
        return Ok(Predicate::Not(alloc::boxed::Box::new(pred_from_json(
            inner,
        )?)));
    }
    let Some((_, field)) = pairs.iter().find(|(k, _)| k == "field") else {
        return Err(QueryError::Shape);
    };
    let Json::String(field) = field else {
        return Err(QueryError::Shape);
    };
    for (k, val) in pairs {
        if k == "field" {
            continue;
        }
        let Some(op) = CmpOp::parse(k) else { continue };
        return Ok(Predicate::Cmp {
            field: field.clone(),
            op,
            value: value_from_json(val)?,
        });
    }
    Err(QueryError::Shape)
}

fn value_from_json(v: &Json) -> Result<Value, QueryError> {
    Ok(match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::String(s) => Value::Text(s.clone()),
        Json::Number(s) => {
            if s.contains('.') || s.contains('e') || s.contains('E') {
                let n: f64 = s.parse().map_err(|_| QueryError::Shape)?;
                Value::Float(n)
            } else {
                let n: i64 = s.parse().map_err(|_| QueryError::Shape)?;
                Value::Int(n)
            }
        }
        Json::Array(xs) => {
            let mut out = Vec::new();
            for x in xs {
                out.push(value_from_json(x)?);
            }
            Value::List(out)
        }
        Json::Object(pairs) => {
            if let Some((_, r)) = pairs.iter().find(|(k, _)| k == "ref") {
                let Json::String(s) = r else {
                    return Err(QueryError::Shape);
                };
                return Value::parse_ref_text(s).map_err(|_| QueryError::Shape);
            }
            let mut m = BTreeMap::new();
            for (k, val) in pairs {
                m.insert(k.clone(), value_from_json(val)?);
            }
            Value::Map(m)
        }
    })
}

fn string_list(v: &Json) -> Result<Vec<String>, QueryError> {
    let Json::Array(xs) = v else {
        return Err(QueryError::Shape);
    };
    let mut out = Vec::new();
    for x in xs {
        let Json::String(s) = x else {
            return Err(QueryError::Shape);
        };
        out.push(s.clone());
    }
    Ok(out)
}

fn order_from_json(v: &Json) -> Result<Vec<OrderKey>, QueryError> {
    let Json::Array(xs) = v else {
        return Err(QueryError::Shape);
    };
    let mut out = Vec::new();
    for x in xs {
        let Json::Object(pairs) = x else {
            return Err(QueryError::Shape);
        };
        let mut field = None;
        let mut dir = OrderDir::Asc;
        for (k, val) in pairs {
            match k.as_str() {
                "field" => {
                    let Json::String(s) = val else {
                        return Err(QueryError::Shape);
                    };
                    field = Some(s.clone());
                }
                "dir" => {
                    let Json::String(s) = val else {
                        return Err(QueryError::Shape);
                    };
                    dir = if s == "desc" {
                        OrderDir::Desc
                    } else {
                        OrderDir::Asc
                    };
                }
                _ => {}
            }
        }
        out.push(OrderKey {
            field: field.ok_or(QueryError::Shape)?,
            dir,
        });
    }
    Ok(out)
}

fn agg_from_json(v: &Json) -> Result<Aggregate, QueryError> {
    let Json::Object(pairs) = v else {
        return Err(QueryError::Shape);
    };
    let mut agg = Aggregate::default();
    for (k, val) in pairs {
        match k.as_str() {
            "count" => {
                let Json::Bool(b) = val else {
                    return Err(QueryError::Shape);
                };
                agg.count = *b;
            }
            "histogram" => {
                let Json::String(s) = val else {
                    return Err(QueryError::Shape);
                };
                agg.histogram = Some(s.clone());
            }
            "mean" => {
                let Json::String(s) = val else {
                    return Err(QueryError::Shape);
                };
                agg.mean = Some(s.clone());
            }
            _ => {}
        }
    }
    Ok(agg)
}

fn json_u64(v: &Json) -> Result<u64, QueryError> {
    let Json::Number(s) = v else {
        return Err(QueryError::Shape);
    };
    s.parse().map_err(|_| QueryError::Shape)
}

/// Used by tests that want a stable digest of a page. Not part of the query language.
pub fn page_fingerprint(page: &Page) -> ContentHash {
    let mut bytes = Vec::new();
    for row in &page.rows {
        for (k, v) in row {
            bytes.extend_from_slice(k.as_bytes());
            if let Ok(c) = v.canonical() {
                bytes.extend_from_slice(&c);
            }
        }
    }
    ContentHash(sha256(&bytes))
}

#[allow(dead_code)]
fn _edge(e: RefEdge) -> RefEdge {
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use aos_types::{Actor, ManualClock, Object, Taint};

    struct Mem {
        objs: Vec<Object>,
    }

    impl ObjectView for Mem {
        fn get(&self, id: &ObjectId) -> Option<Object> {
            self.objs.iter().find(|o| o.id == *id).cloned()
        }
        fn for_each(&self, f: &mut dyn FnMut(&Object)) {
            for o in &self.objs {
                f(o);
            }
        }
        fn reverse(&self, target: &ObjectId) -> Vec<RefEdge> {
            let mut out = Vec::new();
            for o in &self.objs {
                for (field, id) in o.ref_edges() {
                    if id == *target {
                        out.push(RefEdge {
                            source: o.id,
                            field,
                        });
                    }
                }
            }
            out
        }
    }

    #[test]
    fn parses_spec_shape_and_pages() {
        let json = r#"{
          "from": "ml/Sample",
          "where": {"all": [
            {"field": "label", "in": ["fall", "lying"]},
            {"field": "quality_score", "lt": 0.5}
          ]},
          "select": ["id", "label"],
          "order_by": [{"field": "label", "dir": "asc"}],
          "limit": 1
        }"#;
        let q = parse_query(json).unwrap();
        assert_eq!(q.effective_limit(), 1);
        let mut objs = Vec::new();
        for (i, label) in ["fall", "lying", "ok"].iter().enumerate() {
            let mut o = Object::new(
                ObjectId::from_parts(1, i as u128),
                "ml/Sample",
                1,
                Actor::System,
            );
            o.fields
                .insert("label".into(), Value::Text((*label).into()));
            o.fields.insert("quality_score".into(), Value::Float(0.2));
            if *label == "ok" {
                o.fields.insert("quality_score".into(), Value::Float(0.9));
            }
            objs.push(o);
        }
        let page = execute(&Mem { objs }, &q).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert!(page.next_cursor.is_some());
        let _ = ManualClock::new(0);
        let _ = Taint::Clean;
    }
}
