//! Array / page / set iteration functions: `Map`, `Foreach`, `Filter`,
//! `Reduce`, `Take`, `Drop`, `Prepend`, `Append`, `Count`, `Sum`, `Mean`,
//! `Max`, `Min`, `Any`, `All`, `IsEmpty`, `IsNonEmpty`, `Reverse`,
//! `First`, `Last`, `Distinct` (array form).
//!
//! Spec: ext/model/src/main/scala/ast/functions/ArrayFunctions.scala,
//! ReduceFunctions.scala and AbstractFoldFunction.scala. When the
//! collection is a Set these functions operate on the materialised set
//! (see `sets::materialize`), and when it is a Page they preserve the
//! page's cursors.

use super::forms::Registry;

pub fn register(r: &mut Registry) {
    r.stub("map", &["map", "collection"], &[]);
    r.stub("foreach", &["foreach", "collection"], &[]);
    r.stub("filter", &["filter", "collection"], &[]);
    r.stub("reduce", &["reduce", "initial", "collection"], &[]);
    r.stub("prepend", &["prepend", "collection"], &[]);
    r.stub("append", &["append", "collection"], &[]);
    r.stub("take", &["take", "collection"], &[]);
    r.stub("drop", &["drop", "collection"], &[]);
    r.stub("is_empty", &["is_empty"], &[]);
    r.stub("is_nonempty", &["is_nonempty"], &[]);
    r.stub("count", &["count"], &[]);
    r.stub("sum", &["sum"], &[]);
    r.stub("mean", &["mean"], &[]);
    r.stub("max", &["max"], &[]);
    r.stub("min", &["min"], &[]);
    r.stub("any", &["any"], &[]);
    r.stub("all", &["all"], &[]);
    r.stub("reverse", &["reverse"], &[]);
    r.stub("first", &["first"], &[]);
    r.stub("last", &["last"], &[]);
}
