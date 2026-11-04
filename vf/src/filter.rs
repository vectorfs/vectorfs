//! QueryVectors metadata filter engine (M1 T5, design.md §4.6).
//!
//! Three layers over one AST ([`Filter`]):
//! 1. [`Filter::compile`] — structural validation. Operator set is the AWS
//!    full set (verified against the S3 Vectors user guide
//!    "Metadata filtering", 2026-11-04): `$eq $ne $gt $gte $lt $lte $startsWith
//!    $in $nin $exists $and $or` — there is no `$nor` (that is Aliyun-only).
//!    Operator-value typing follows the AWS operator table: `$eq/$ne` take
//!    string|number|boolean, `$gt/$gte/$lt/$lte` take number, `$startsWith`
//!    takes string, `$in/$nin` take a non-empty array of primitives, `$exists`
//!    takes boolean, `$and/$or` take non-empty arrays of filters (nested to
//!    any depth). Top-level multiple keys = implicit AND; `{"field": scalar}`
//!    is `$eq` shorthand; multiple operators on one field = AND. Structural
//!    violations (typo'd operator name, empty `$and`, non-object filter, ...)
//!    are ValidationException(fieldList) material — the error type carries the
//!    message and the ops layer attaches `path: "filter"`.
//!    AWS also caps ENHANCED filtered queries at 100 "constraints" (each value
//!    evaluated counts one: a 3-element `$in` is 3); our runtime is always
//!    ENHANCED (design.md §4.4) so the cap is enforced here.
//!
//! 2. [`Filter::eval`] — the exact in-memory evaluator (the semantic
//!    reference). Boundary semantics pinned by tests (TODO(AWS 对拍) where the
//!    service behavior is not documented):
//!
//!    | stored \ filter        | missing field        | type mismatch     | list stored value              |
//!    |------------------------|----------------------|-------------------|--------------------------------|
//!    | `$eq`                  | false                | false             | any element equals             |
//!    | `$ne`                  | true (Mongo)         | true              | no element equals              |
//!    | `$gt/$gte/$lt/$lte`    | false                | false             | false (numbers only, no        |
//!    |                        |                      |                   | any-element; AWS documents it  |
//!    |                        |                      |                   | only for $eq/$in)              |
//!    | `$startsWith`          | false                | false             | false (string only)            |
//!    | `$in`                  | false                | element-wise      | any element in list            |
//!    | `$nin`                 | true (Mongo)         | element-wise      | no element in list             |
//!    | `$exists`              | false / true directly (field presence; nulls are never stored, design §4.4) |
//!
//!    "Type mismatch = no match, not an error" and "$ne/$nin match missing
//!    fields" are the MongoDB conventions the task pinned as the conservative
//!    choice; AWS does not document either.
//!
//! 3. [`Filter::pushdown`] — translate the AST into a DataFusion expression
//!    over the `metadata` struct's dynamic mirror columns (see store.rs) for
//!    lance-side pre-filtering (ENHANCED pre-filter semantics: filter first,
//!    then KNN). The translation is a *necessary condition*: it may over-match
//!    (drop nothing the exact evaluator would keep), never under-match, so the
//!    caller always re-runs [`Filter::eval`] on the surviving candidates.
//!    Predicates that cannot be expressed (field has no mirror column — e.g.
//!    list-valued or non-filterable keys — or the literal type disagrees with
//!    the column type) are skipped; an `$or` with any non-translatable branch
//!    is entirely skipped. Nested field access uses lance's own idiom
//!    (`get_field(metadata, lit(key))` — the same UDF lance's scanner uses
//!    for nested paths, verified against lancedb 0.41 / lance 14.0.2).

use std::collections::BTreeMap;

use lancedb::expr::{DfExpr, col, is_in, lit};

/// AWS "Filter constraints per QueryVectors request" cap on ENHANCED indexes
/// (S3 Vectors limitations page, 2026-11-04). Our runtime is always ENHANCED.
pub const MAX_CONSTRAINTS: usize = 100;

/// A primitive filter value (operator input). Nulls are rejected at compile
/// time: AWS documents only String/Number/Boolean inputs and metadata never
/// stores nulls, so a null could only ever mean "field missing" — unspecified
/// behavior we fail closed on. TODO(AWS 对拍): real-service handling of null
/// filter values.
#[derive(Debug, Clone, PartialEq)]
pub enum FVal {
    Num(f64),
    Str(String),
    Bool(bool),
}

impl FVal {
    /// JSON type name for error messages.
    #[allow(dead_code)] // kept for future per-value error text
    fn type_name(&self) -> &'static str {
        match self {
            FVal::Num(_) => "number",
            FVal::Str(_) => "string",
            FVal::Bool(_) => "boolean",
        }
    }
}

/// One operator condition on a field.
#[derive(Debug, Clone)]
pub enum Cond {
    Eq(FVal),
    Ne(FVal),
    /// $gt/$gte/$lt/$lte — filter input is number-only (AWS operator table).
    Gt(f64),
    Gte(f64),
    Lt(f64),
    Lte(f64),
    StartsWith(String),
    In(Vec<FVal>),
    Nin(Vec<FVal>),
    Exists(bool),
}

/// Filter tree node. Top-level [`Filter`] is an implicit AND over nodes.
#[derive(Debug, Clone)]
pub enum Node {
    Field(String, Vec<Cond>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

/// Compiled, validated filter. Cheap to clone per-row evaluate; `eval` walks
/// the (small) tree directly.
#[derive(Debug, Clone)]
pub struct Filter(Vec<Node>);

/// Compile error -> ValidationException (message only; ops adds fieldList).
#[derive(Debug)]
pub struct FilterError(pub String);

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn err(msg: impl Into<String>) -> FilterError {
    FilterError(msg.into())
}

impl Filter {
    /// Parse + validate a request `filter` document (JSON). `Null`/absent is
    /// handled by the caller (no filter); an empty object `{}` is a valid
    /// no-op filter (matches everything).
    pub fn compile(v: &serde_json::Value) -> Result<Filter, FilterError> {
        let nodes = compile_object(v, "filter")?;
        let f = Filter(nodes);
        let n = f.constraint_count();
        if n > MAX_CONSTRAINTS {
            return Err(err(format!(
                "filter uses {n} constraints; the maximum per query is {MAX_CONSTRAINTS} \
                 (each evaluated value counts as one constraint)"
            )));
        }
        Ok(f)
    }

    /// Exact evaluation against one vector's metadata document (the
    /// semantic reference — see the module doc boundary table).
    pub fn eval(&self, meta: &serde_json::Value) -> bool {
        self.0.iter().all(|n| eval_node(n, meta))
    }

    /// AWS constraint accounting: each evaluated value counts one — a scalar
    /// condition is 1, `$in`/`$nin` count their elements, `$and`/`$or` sum
    /// their children (AWS metadata-filtering page, 2026-11-04).
    pub fn constraint_count(&self) -> usize {
        self.0.iter().map(node_constraints).sum()
    }

    /// Every field name referenced anywhere in the filter (used by ops to
    /// reject filters over `nonFilterableMetadataKeys` keys).
    pub fn fields(&self) -> Vec<&str> {
        let mut out = Vec::new();
        collect_fields(&self.0, &mut out);
        out
    }

    /// Translate into a DataFusion predicate over the mirror columns.
    ///
    /// `cols` maps field name -> mirror column kind for every mirrored key
    /// (scalar-typed filterable keys; list-valued and non-filterable keys are
    /// absent by construction). Returns None when nothing is translatable —
    /// the caller then falls back to the pure in-memory path.
    ///
    /// Safety property (pinned by the dual-path consistency test): the
    /// returned predicate may match MORE rows than the exact filter, never
    /// fewer. See the per-operator reasoning in [`pushdown_cond`].
    pub fn pushdown(&self, cols: &BTreeMap<String, MetaCol>) -> Option<DfExpr> {
        conjoin(self.0.iter().filter_map(|n| pushdown_node(n, cols)))
    }

    /// Canonical compact JSON (fingerprint input for QueryVectors pagination
    /// tokens — a follow-up page must present the same filter).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Mirror column kinds (see store.rs — one typed child of the `metadata`
/// struct per scalar filterable key; number is always Float64 so that 7 and
/// 7.5 coexist under one column without int/float drift).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaCol {
    F64,
    Utf8,
    Bool,
}

// ---------------------------------------------------------------------------
// compile (structural validation)
// ---------------------------------------------------------------------------

const KNOWN_OPS: &[&str] = &[
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$startsWith",
    "$in",
    "$nin",
    "$exists",
];

fn compile_object(v: &serde_json::Value, where_: &str) -> Result<Vec<Node>, FilterError> {
    let serde_json::Value::Object(map) = v else {
        return Err(err(format!(
            "{where_} must be a JSON object mapping fields to conditions, got {}",
            json_type(v)
        )));
    };
    let mut nodes = Vec::with_capacity(map.len());
    for (k, cond) in map {
        if let Some(op) = k.strip_prefix('$') {
            match op {
                "and" | "or" => {
                    let serde_json::Value::Array(items) = cond else {
                        return Err(err(format!("${op} must be an array of filter objects")));
                    };
                    if items.is_empty() {
                        return Err(err(format!(
                            "${op} must contain at least one filter object"
                        )));
                    }
                    let children = items
                        .iter()
                        .enumerate()
                        .map(|(i, it)| {
                            // each element compiles to a single-node implicit AND
                            let nodes = compile_object(it, &format!("${op}[{i}]"))?;
                            Ok(Node::And(nodes))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    nodes.push(if op == "and" {
                        Node::And(children)
                    } else {
                        Node::Or(children)
                    });
                }
                _ => {
                    return Err(err(format!(
                        "unknown filter operator {k:?} (supported operators: {})",
                        KNOWN_OPS.join(", ")
                    )));
                }
            }
        } else {
            if k.is_empty() {
                return Err(err("filter field name must not be empty"));
            }
            let conds = compile_field(k, cond)?;
            nodes.push(Node::Field(k.clone(), conds));
        }
    }
    Ok(nodes)
}

fn compile_field(field: &str, cond: &serde_json::Value) -> Result<Vec<Cond>, FilterError> {
    // shorthand {"field": scalar} == {"field": {"$eq": scalar}}
    let serde_json::Value::Object(map) = cond else {
        let v = parse_primitive(cond).ok_or_else(|| {
            err(format!(
                "filter.{field} shorthand value must be a string, number or boolean \
                 (implicit $eq), got {}",
                json_type(cond)
            ))
        })?;
        return Ok(vec![Cond::Eq(v)]);
    };
    if map.is_empty() {
        return Err(err(format!(
            "filter.{field} condition object must not be empty (supported operators: {})",
            KNOWN_OPS.join(", ")
        )));
    }
    let mut out = Vec::with_capacity(map.len());
    for (op, arg) in map {
        let c = match op.as_str() {
            "$eq" => Cond::Eq(value_arg(field, op, arg)?),
            "$ne" => Cond::Ne(value_arg(field, op, arg)?),
            "$gt" => Cond::Gt(num_arg(field, op, arg)?),
            "$gte" => Cond::Gte(num_arg(field, op, arg)?),
            "$lt" => Cond::Lt(num_arg(field, op, arg)?),
            "$lte" => Cond::Lte(num_arg(field, op, arg)?),
            "$startsWith" => {
                let serde_json::Value::String(s) = arg else {
                    return Err(err(format!(
                        "filter.{field}.{op} requires a string value, got {}",
                        json_type(arg)
                    )));
                };
                Cond::StartsWith(s.clone())
            }
            "$in" | "$nin" => {
                let serde_json::Value::Array(items) = arg else {
                    return Err(err(format!(
                        "filter.{field}.{op} requires an array of values, got {}",
                        json_type(arg)
                    )));
                };
                if items.is_empty() {
                    return Err(err(format!("filter.{field}.{op} array must not be empty")));
                }
                let vals = items
                    .iter()
                    .map(|i| {
                        parse_primitive(i).ok_or_else(|| {
                            err(format!(
                                "filter.{field}.{op} elements must be string, number or \
                                 boolean values, got {}",
                                json_type(i)
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if op == "$in" {
                    Cond::In(vals)
                } else {
                    Cond::Nin(vals)
                }
            }
            "$exists" => {
                let serde_json::Value::Bool(b) = arg else {
                    return Err(err(format!(
                        "filter.{field}.{op} requires a boolean value, got {}",
                        json_type(arg)
                    )));
                };
                Cond::Exists(*b)
            }
            other => {
                return Err(err(format!(
                    "unknown filter operator {other:?} on field {field:?} (supported \
                     operators: {})",
                    KNOWN_OPS.join(", ")
                )));
            }
        };
        out.push(c);
    }
    Ok(out)
}

fn value_arg(field: &str, op: &str, arg: &serde_json::Value) -> Result<FVal, FilterError> {
    parse_primitive(arg).ok_or_else(|| {
        err(format!(
            "filter.{field}.{op} requires a string, number or boolean value, got {}",
            json_type(arg)
        ))
    })
}

fn num_arg(field: &str, op: &str, arg: &serde_json::Value) -> Result<f64, FilterError> {
    let serde_json::Value::Number(n) = arg else {
        return Err(err(format!(
            "filter.{field}.{op} requires a number value, got {}",
            json_type(arg)
        )));
    };
    n.as_f64()
        .ok_or_else(|| err(format!("filter.{field}.{op} value is not a finite number")))
}

/// Operator-input primitive: string / number / boolean. Null, arrays and
/// objects are rejected (AWS operator table types; fail-closed on null —
/// see FVal doc).
fn parse_primitive(v: &serde_json::Value) -> Option<FVal> {
    match v {
        serde_json::Value::String(s) => Some(FVal::Str(s.clone())),
        serde_json::Value::Number(n) => n.as_f64().map(FVal::Num),
        serde_json::Value::Bool(b) => Some(FVal::Bool(*b)),
        _ => None,
    }
}

fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "object",
    }
}

fn node_constraints(n: &Node) -> usize {
    match n {
        Node::Field(_, conds) => conds
            .iter()
            .map(|c| match c {
                Cond::In(vs) | Cond::Nin(vs) => vs.len(),
                _ => 1,
            })
            .sum(),
        Node::And(children) | Node::Or(children) => children.iter().map(node_constraints).sum(),
    }
}

fn collect_fields<'a>(nodes: &'a [Node], out: &mut Vec<&'a str>) {
    for n in nodes {
        match n {
            Node::Field(f, _) => out.push(f),
            Node::And(c) | Node::Or(c) => collect_fields(c, out),
        }
    }
}

// ---------------------------------------------------------------------------
// exact evaluation
// ---------------------------------------------------------------------------

fn eval_node(n: &Node, meta: &serde_json::Value) -> bool {
    match n {
        Node::Field(f, conds) => conds
            .iter()
            .all(|c| eval_cond(c, meta.get(f).filter(|v| !v.is_null()))),
        Node::And(c) => c.iter().all(|n| eval_node(n, meta)),
        Node::Or(c) => c.iter().any(|n| eval_node(n, meta)),
    }
}

/// `stored` is Some only when the field exists with a non-null value (nulls
/// are never stored, but a defensive read treats them as missing).
fn eval_cond(c: &Cond, stored: Option<&serde_json::Value>) -> bool {
    match c {
        Cond::Eq(v) => stored.is_some_and(|s| eq_any(v, s)),
        Cond::Ne(v) => !stored.is_some_and(|s| eq_any(v, s)),
        Cond::Gt(x) => as_f64(stored).is_some_and(|s| s > *x),
        Cond::Gte(x) => as_f64(stored).is_some_and(|s| s >= *x),
        Cond::Lt(x) => as_f64(stored).is_some_and(|s| s < *x),
        Cond::Lte(x) => as_f64(stored).is_some_and(|s| s <= *x),
        Cond::StartsWith(p) => match stored {
            Some(serde_json::Value::String(s)) => s.starts_with(p.as_str()),
            _ => false, // missing, or non-string incl. list: no any-element for $startsWith
        },
        Cond::In(vs) => stored.is_some_and(|s| vs.iter().any(|v| eq_any(v, s))),
        Cond::Nin(vs) => !stored.is_some_and(|s| vs.iter().any(|v| eq_any(v, s))),
        Cond::Exists(want) => stored.is_some() == *want,
    }
}

/// Scalar equality with strict typing: numbers compare as f64 (1 == 1.0),
/// string/boolean strictly; against a stored LIST any element may match
/// (AWS: "$eq ... When comparing with an array metadata value, returns true
/// if the input value matches any element in the array"; $in/$nin inherit
/// the same any-element semantics).
fn eq_any(want: &FVal, stored: &serde_json::Value) -> bool {
    match stored {
        serde_json::Value::Array(items) => items.iter().any(|i| eq_any(want, i)),
        serde_json::Value::Number(n) => match want {
            FVal::Num(w) => n.as_f64().is_some_and(|s| s == *w),
            _ => false,
        },
        serde_json::Value::String(s) => matches!(want, FVal::Str(w) if w == s),
        serde_json::Value::Bool(b) => matches!(want, FVal::Bool(w) if w == b),
        _ => false,
    }
}

/// Numeric view of the stored value (comparisons are number-only per the AWS
/// operator table; a non-number stored value — string, bool, list — simply
/// does not compare).
fn as_f64(stored: Option<&serde_json::Value>) -> Option<f64> {
    match stored {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// pushdown translation (necessary condition; never under-matches)
// ---------------------------------------------------------------------------

/// Nested mirror-column reference: `get_field(metadata, lit(field))` —
/// lance's own nested-path idiom (lance 14 scanner.rs uses this same UDF);
/// verified live against lancedb 0.41 (M1 T5 probe, 2026-11-04).
fn mcol(field: &str) -> DfExpr {
    use lancedb::datafusion::functions::core::get_field;
    get_field().call(vec![col("metadata"), lit(field)])
}

fn conjoin(mut parts: impl Iterator<Item = DfExpr>) -> Option<DfExpr> {
    parts
        .next()
        .map(|first| parts.fold(first, |acc, e| acc.and(e)))
}

fn disjoin(mut parts: impl Iterator<Item = DfExpr>) -> Option<DfExpr> {
    parts
        .next()
        .map(|first| parts.fold(first, |acc, e| acc.or(e)))
}

fn pushdown_node(n: &Node, cols: &BTreeMap<String, MetaCol>) -> Option<DfExpr> {
    match n {
        Node::And(children) => conjoin(children.iter().filter_map(|c| pushdown_node(c, cols))),
        // ALL branches must translate, else skip the whole $or: a dropped
        // (untranslatable) branch would remove rows the exact filter keeps.
        Node::Or(children) => {
            let all: Vec<Option<DfExpr>> =
                children.iter().map(|c| pushdown_node(c, cols)).collect();
            if all.iter().any(Option::is_none) {
                None
            } else {
                disjoin(all.into_iter().map(|o| o.expect("none-ness checked above")))
            }
        }
        Node::Field(f, conds) => {
            // no mirror column (list-valued key, non-filterable key, unseen
            // key): skip — the exact post-filter still applies to candidates.
            let kind = *cols.get(f)?;
            conjoin(
                conds
                    .iter()
                    .filter_map(|c| pushdown_cond(&mcol(f), c, kind)),
            )
        }
    }
}

fn pushdown_cond(cexpr: &DfExpr, c: &Cond, kind: MetaCol) -> Option<DfExpr> {
    let matches_kind = |v: &FVal| {
        matches!(
            (kind, v),
            (MetaCol::F64, FVal::Num(_))
                | (MetaCol::Utf8, FVal::Str(_))
                | (MetaCol::Bool, FVal::Bool(_))
        )
    };
    match c {
        // EXACT for mirrored columns: the mirror is non-null iff the blob has
        // a scalar value of exactly this type (store.rs writes both together),
        // and engine `NULL = lit` is false — matching our missing->false.
        Cond::Eq(v) if matches_kind(v) => Some(cexpr.clone().eq(lit_val(v))),
        // memory semantics: $ne matches missing fields and mismatched types;
        // only rows whose mirror holds an equal same-type value are excluded.
        // SQL 3VL traps: bare `col != v` drops NULL rows (NULL != v = NULL),
        // and `IS NOT NULL AND !=` drops them too — the exact form is
        // `col IS NULL OR col != v`.
        Cond::Ne(v) if matches_kind(v) => {
            Some(cexpr.clone().is_null().or(cexpr.clone().not_eq(lit_val(v))))
        }
        // EXACT: comparisons are number-only and the mirror is Float64, so a
        // non-null mirror is always a number; null mirrors (missing or
        // non-number values) compare false in SQL — matching memory semantics.
        Cond::Gt(x) if kind == MetaCol::F64 => Some(cexpr.clone().gt(lit(*x))),
        Cond::Gte(x) if kind == MetaCol::F64 => Some(cexpr.clone().gt_eq(lit(*x))),
        Cond::Lt(x) if kind == MetaCol::F64 => Some(cexpr.clone().lt(lit(*x))),
        Cond::Lte(x) if kind == MetaCol::F64 => Some(cexpr.clone().lt_eq(lit(*x))),
        // EXACT prefix via UTF-8 range: {s : s >= p} \ {s : s >= upper(p)}
        // where upper(p) increments p's last representable char. Arrow string
        // comparison is byte-lexicographic = codepoint order, so this is
        // precisely startsWith. (Keeps $startsWith pushdown free of SQL LIKE
        // escaping; exactness also means no post-filter reliance.)
        Cond::StartsWith(p) if kind == MetaCol::Utf8 => Some(match prefix_upper(p) {
            Some(upper) => cexpr
                .clone()
                .gt_eq(lit(p.clone()))
                .and(cexpr.clone().lt(lit(upper))),
            None => cexpr.clone().gt_eq(lit(p.clone())), // prefix is maximal: >= is exact
        }),
        // EXACT: literals of a different type can never equal a non-null
        // mirror of `kind` (strict typing on both sides), so dropping them
        // from the pushed IN is safe. An all-mismatched list (empty subset)
        // matches nothing engine-side (missing -> $in false as well) — return
        // None and let the exact filter handle it uniformly.
        Cond::In(vs) => {
            let same: Vec<_> = vs.iter().filter(|v| matches_kind(v)).cloned().collect();
            if same.is_empty() {
                None
            } else {
                Some(is_in(cexpr.clone(), same.iter().map(lit_val).collect()))
            }
        }
        // memory: match iff stored scalar NOT in list (missing rows match).
        // Push: mirror IS NULL OR mirror NOT IN (same-type literals) — exact
        // for the same strict-typing reason as $in.
        Cond::Nin(vs) => {
            let same: Vec<_> = vs.iter().filter(|v| matches_kind(v)).cloned().collect();
            if same.is_empty() {
                None // nothing can exclude any row
            } else {
                Some(
                    cexpr.clone().is_null().or(cexpr
                        .clone()
                        .in_list(same.iter().map(lit_val).collect(), true)),
                )
            }
        }
        // EXACT for mirrored columns (mirror null <=> field absent).
        Cond::Exists(true) => Some(cexpr.clone().is_not_null()),
        Cond::Exists(false) => Some(cexpr.clone().is_null()),
        // everything else (type-mismatched literals): no narrowing possible.
        _ => None,
    }
}

fn lit_val(v: &FVal) -> DfExpr {
    match v {
        FVal::Num(n) => lit(*n),
        FVal::Str(s) => lit(s.clone()),
        FVal::Bool(b) => lit(*b),
    }
}

/// Smallest string greater than every string starting with `p` (None when no
/// such bound exists — `p` consists only of U+10FFFF chars, in which case
/// every string >= p starts with p).
fn prefix_upper(p: &str) -> Option<String> {
    let mut chars: Vec<char> = p.chars().collect();
    while let Some(&last) = chars.last() {
        if (last as u32) < char::MAX as u32 {
            let next = char::from_u32(last as u32 + 1).expect("last < MAX so +1 is valid");
            chars.pop();
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
        chars.pop(); // U+10FFFF: carry into the previous char
    }
    None
}

// ---------------------------------------------------------------------------
// serde (canonical JSON for pagination fingerprints)
// ---------------------------------------------------------------------------

impl serde::Serialize for Filter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for Filter {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let nodes = Vec::<Node>::deserialize(d)?;
        Ok(Filter(nodes))
    }
}

impl serde::Serialize for Cond {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(1))?;
        match self {
            Cond::Eq(v) => map.serialize_entry("$eq", v)?,
            Cond::Ne(v) => map.serialize_entry("$ne", v)?,
            Cond::Gt(x) => map.serialize_entry("$gt", x)?,
            Cond::Gte(x) => map.serialize_entry("$gte", x)?,
            Cond::Lt(x) => map.serialize_entry("$lt", x)?,
            Cond::Lte(x) => map.serialize_entry("$lte", x)?,
            Cond::StartsWith(p) => map.serialize_entry("$startsWith", p)?,
            Cond::In(vs) => map.serialize_entry("$in", vs)?,
            Cond::Nin(vs) => map.serialize_entry("$nin", vs)?,
            Cond::Exists(b) => map.serialize_entry("$exists", b)?,
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for Cond {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let m = serde_json::Map::deserialize(d)?;
        let mut it = m.into_iter();
        let (k, v) = it
            .next()
            .ok_or_else(|| serde::de::Error::custom("empty condition"))?;
        if it.next().is_some() {
            return Err(serde::de::Error::custom("multiple operators in one Cond"));
        }
        Ok(match k.as_str() {
            "$eq" => Cond::Eq(serde_json::from_value(v).map_err(de_err)?),
            "$ne" => Cond::Ne(serde_json::from_value(v).map_err(de_err)?),
            "$gt" => Cond::Gt(num_value(&v)?),
            "$gte" => Cond::Gte(num_value(&v)?),
            "$lt" => Cond::Lt(num_value(&v)?),
            "$lte" => Cond::Lte(num_value(&v)?),
            "$startsWith" => Cond::StartsWith(
                v.as_str()
                    .ok_or_else(|| serde::de::Error::custom("not a string"))?
                    .to_string(),
            ),
            "$in" => Cond::In(val_list(&v)?),
            "$nin" => Cond::Nin(val_list(&v)?),
            "$exists" => Cond::Exists(
                v.as_bool()
                    .ok_or_else(|| serde::de::Error::custom("not a boolean"))?,
            ),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown operator {other:?}"
                )));
            }
        })
    }
}

/// Deserialize-helper: a number argument (D::Error-typed, no serde_json dep).
fn de_err<E: serde::de::Error>(e: serde_json::Error) -> E {
    E::custom(e)
}

fn num_value<E: serde::de::Error>(v: &serde_json::Value) -> Result<f64, E> {
    v.as_f64().ok_or_else(|| E::custom("not a number"))
}

/// Deserialize-helper: an array of primitives (D::Error-typed).
fn val_list<E: serde::de::Error>(v: &serde_json::Value) -> Result<Vec<FVal>, E> {
    let arr = v.as_array().ok_or_else(|| E::custom("not an array"))?;
    arr.iter()
        .map(|i| parse_primitive(i).ok_or_else(|| E::custom("not a primitive value")))
        .collect()
}

impl serde::Serialize for FVal {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            FVal::Num(n) => s.serialize_f64(*n),
            FVal::Str(st) => s.serialize_str(st),
            FVal::Bool(b) => s.serialize_bool(*b),
        }
    }
}

impl<'de> serde::Deserialize<'de> for FVal {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        parse_primitive(&v).ok_or_else(|| serde::de::Error::custom("not a primitive value"))
    }
}

impl serde::Serialize for Node {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Node::Field(f, conds) => {
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry(f, &conds)?;
                map.end()
            }
            Node::And(children) => {
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry("$and", children)?;
                map.end()
            }
            Node::Or(children) => {
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry("$or", children)?;
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let m = serde_json::Map::deserialize(d)?;
        let mut it = m.into_iter();
        let (k, v) = it
            .next()
            .ok_or_else(|| serde::de::Error::custom("empty node"))?;
        if it.next().is_some() {
            return Err(serde::de::Error::custom("multiple keys in one node"));
        }
        Ok(match k.as_str() {
            "$and" | "$or" => {
                let children: Vec<Node> = serde_json::from_value(v).map_err(de_err)?;
                if children.is_empty() {
                    return Err(serde::de::Error::custom("empty $and/$or"));
                }
                if k == "$and" {
                    Node::And(children)
                } else {
                    Node::Or(children)
                }
            }
            field => Node::Field(
                field.to_string(),
                serde_json::from_value::<Vec<Cond>>(v).map_err(de_err)?,
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// unit tests: operator semantics + boundary table + validation errors
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta() -> serde_json::Value {
        json!({
            "s": "apple", "n": 7, "f": 2.5, "b": true,
            "tags": ["x", "y", 3],
            "nums": [1, 2.5],
        })
    }

    #[test]
    fn compile_and_eval_full_operator_set() {
        let m = meta();
        let cases: Vec<(&str, serde_json::Value, bool)> = vec![
            // $eq across types + shorthand equivalence
            ("eq str", json!({"s": "apple"}), true),
            ("eq str miss", json!({"s": "banana"}), false),
            ("eq str explicit", json!({"s": {"$eq": "apple"}}), true),
            ("eq num int/float", json!({"n": 7.0}), true),
            ("eq num miss", json!({"n": 8}), false),
            ("eq float", json!({"f": 2.5}), true),
            ("eq bool", json!({"b": true}), true),
            ("eq bool miss", json!({"b": false}), false),
            // type mismatch = no match, no error
            ("eq mismatch str-on-num", json!({"n": "7"}), false),
            ("eq mismatch num-on-str", json!({"s": 7}), false),
            ("eq mismatch bool-on-num", json!({"n": true}), false),
            // missing field
            ("eq missing", json!({"ghost": "x"}), false),
            ("eq null-stored", json!({"nullish": 1}), false),
            // list metadata any-element ($eq/$in/$nin only)
            ("eq list anyelem", json!({"tags": "y"}), true),
            ("eq list anyelem num", json!({"tags": 3}), true),
            ("eq list nomatch", json!({"tags": "z"}), false),
            ("eq list nums", json!({"nums": 2.5}), true),
            // $ne
            ("ne present", json!({"s": {"$ne": "pear"}}), true),
            ("ne equal", json!({"s": {"$ne": "apple"}}), false),
            ("ne missing -> true", json!({"ghost": {"$ne": "x"}}), true),
            ("ne mismatch -> true", json!({"n": {"$ne": "7"}}), true),
            ("ne on list", json!({"tags": {"$ne": "y"}}), false),
            ("ne on list nomatch", json!({"tags": {"$ne": "z"}}), true),
            // comparisons
            ("gt", json!({"n": {"$gt": 6}}), true),
            ("gt eq-boundary", json!({"n": {"$gt": 7}}), false),
            ("gte boundary", json!({"n": {"$gte": 7}}), true),
            ("lt", json!({"n": {"$lt": 7}}), false),
            ("lte", json!({"n": {"$lte": 7}}), true),
            ("gt float", json!({"f": {"$gt": 2.4}}), true),
            ("gt missing", json!({"ghost": {"$gt": 0}}), false),
            ("gt non-number stored", json!({"s": {"$gt": 0}}), false),
            // $startsWith
            ("startsWith", json!({"s": {"$startsWith": "app"}}), true),
            (
                "startsWith full",
                json!({"s": {"$startsWith": "apple"}}),
                true,
            ),
            (
                "startsWith miss",
                json!({"s": {"$startsWith": "apl"}}),
                false,
            ),
            (
                "startsWith missing",
                json!({"ghost": {"$startsWith": "a"}}),
                false,
            ),
            (
                "startsWith non-string stored",
                json!({"n": {"$startsWith": "7"}}),
                false,
            ),
            // $in / $nin
            ("in hit", json!({"s": {"$in": ["pear", "apple"]}}), true),
            ("in miss", json!({"s": {"$in": ["pear", "fig"]}}), false),
            ("in missing field", json!({"ghost": {"$in": ["a"]}}), false),
            (
                "in mixed types",
                json!({"s": {"$in": ["apple", 7, true]}}),
                true,
            ),
            ("in num", json!({"n": {"$in": [6, 7, 8]}}), true),
            (
                "in list anyelem",
                json!({"tags": {"$in": ["z", "x"]}}),
                true,
            ),
            (
                "in list nomatch",
                json!({"tags": {"$in": ["z", "w"]}}),
                false,
            ),
            ("nin hit", json!({"s": {"$nin": ["pear", "fig"]}}), true),
            (
                "nin excluded",
                json!({"s": {"$nin": ["pear", "apple"]}}),
                false,
            ),
            (
                "nin missing -> true",
                json!({"ghost": {"$nin": ["a"]}}),
                true,
            ),
            ("nin mismatch -> true", json!({"n": {"$nin": ["7"]}}), true),
            (
                "nin list anyelem",
                json!({"tags": {"$nin": ["z", "x"]}}),
                false,
            ),
            (
                "nin list nomatch",
                json!({"tags": {"$nin": ["z", "w"]}}),
                true,
            ),
            // $exists
            ("exists true", json!({"s": {"$exists": true}}), true),
            (
                "exists true miss",
                json!({"ghost": {"$exists": true}}),
                false,
            ),
            ("exists false", json!({"ghost": {"$exists": false}}), true),
            (
                "exists false present",
                json!({"s": {"$exists": false}}),
                false,
            ),
            ("exists on list", json!({"tags": {"$exists": true}}), true),
            // same field multiple operators = AND
            (
                "range same field",
                json!({"n": {"$gte": 3, "$lte": 10}}),
                true,
            ),
            (
                "range same field excl",
                json!({"n": {"$gte": 8, "$lte": 10}}),
                false,
            ),
            // implicit top-level AND
            ("implicit and", json!({"s": "apple", "n": {"$gt": 5}}), true),
            (
                "implicit and excl",
                json!({"s": "apple", "n": {"$gt": 50}}),
                false,
            ),
            // $and / $or, nested
            ("and", json!({"$and": [{"s": "apple"}, {"n": 7}]}), true),
            (
                "and excl",
                json!({"$and": [{"s": "apple"}, {"n": 8}]}),
                false,
            ),
            ("or", json!({"$or": [{"s": "pear"}, {"n": 7}]}), true),
            ("or excl", json!({"$or": [{"s": "pear"}, {"n": 8}]}), false),
            (
                "deep nest",
                json!({"$or": [{"$and": [{"s": "apple"}, {"n": {"$lt": 5}}]}, {"b": true}]}),
                true,
            ),
            (
                "deep nest excl",
                json!({"$or": [{"$and": [{"s": "apple"}, {"n": {"$lt": 5}}]}, {"b": false}]}),
                false,
            ),
            // empty filter object = match-all
            ("empty object", json!({}), true),
        ];
        for (name, f, want) in cases {
            let compiled = Filter::compile(&f).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(compiled.eval(&m), want, "case {name}: {f}");
        }
    }

    #[test]
    fn validation_errors() {
        let bad = vec![
            json!("not an object"),
            json!([{"s": "x"}]),
            json!({"$regex": "x.*"}),
            json!({"$nor": [{"s": "x"}]}),
            json!({"$and": []}),
            json!({"$or": []}),
            json!({"$and": {"s": "x"}}),
            json!({"$and": ["scalar"]}),
            json!({"s": {"$typo": 1}}),
            json!({"s": {}}),
            json!({"": "x"}),
            json!({"s": null}),
            json!({"s": ["x"]}), // shorthand list = $eq with array input
            json!({"n": {"$gt": "7"}}),
            json!({"s": {"$startsWith": 7}}),
            json!({"s": {"$in": []}}),
            json!({"s": {"$in": {}}}),
            json!({"s": {"$in": ["a", null]}}),
            json!({"s": {"$nin": []}}),
            json!({"s": {"$exists": "true"}}),
        ];
        for f in bad {
            assert!(Filter::compile(&f).is_err(), "expected error: {f}");
        }
        // constraint accounting: 1 (scalar) + 3 ($in elements) = 4
        let f = Filter::compile(&json!({"s": "a", "t": {"$in": ["x", "y", "z"]}})).unwrap();
        assert_eq!(f.constraint_count(), 4);
        // over the cap
        let many = json!({"s": {"$in": (0..MAX_CONSTRAINTS + 1).collect::<Vec<_>>()}});
        assert!(Filter::compile(&many).is_err());
        // fields() collection incl. nesting
        let f = Filter::compile(&json!({
            "$or": [{"a": 1}, {"$and": [{"b": 2}, {"c": 3}]}]
        }))
        .unwrap();
        assert_eq!(f.fields(), vec!["a", "b", "c"]);
    }

    #[test]
    fn fingerprint_json_roundtrip() {
        let f = Filter::compile(&json!({"s": "a", "n": {"$gte": 1, "$lt": 9},
            "$or": [{"t": {"$in": ["x", "y"]}}, {"g": {"$exists": false}}]}))
        .unwrap();
        let j = f.to_json();
        let back: Filter = serde_json::from_value(j.clone()).unwrap();
        let m = json!({"s": "a", "n": 4, "t": "y"});
        assert_eq!(back.eval(&m), f.eval(&m));
        assert_eq!(back.to_json(), j, "canonical JSON must be stable");
    }

    #[test]
    fn prefix_upper_bound() {
        assert_eq!(prefix_upper("abc").as_deref(), Some("abd"));
        assert_eq!(prefix_upper("abz").as_deref(), Some("ab{")); // z+1 = {, a valid non-minimal bound
        assert_eq!(prefix_upper("a").as_deref(), Some("b"));
        // carry over U+10FFFF
        let maxp = format!("a{}", char::MAX);
        assert_eq!(prefix_upper(&maxp).as_deref(), Some("b"));
        let allmax = format!("{}", char::MAX);
        assert_eq!(prefix_upper(&allmax), None);
        // strings the bound must include/exclude
        let p = "ab";
        let u = prefix_upper(p).unwrap();
        assert!("ab" < u.as_str() && "abc" < u.as_str() && "ab\u{10FFFF}" < u.as_str());
        assert!("ac" >= u.as_str());
    }
}
