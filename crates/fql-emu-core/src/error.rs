//! Error model matching Fauna's v4 HTTP error shape.
//!
//! Codes, descriptions and status codes mirror `fauna.ast.Error`
//! (ext/model/src/main/scala/ast/Error.scala) and
//! `RenderError.encodeErrors` (ext/model/src/main/scala/Renderable.scala).
//! Keep the strings byte-identical to the server where possible: the
//! JS driver and application code branch on `code` and sometimes on
//! `description`.

use crate::value::Type;
use serde_json::{json, Value as Json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PosElem {
    Index(usize),
    Key(String),
}

/// Path from the query root to the sub-expression an error refers to,
/// rendered as e.g. `["let", 0, "x", "select"]`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Position(pub Vec<PosElem>);

impl Position {
    pub fn root() -> Position {
        Position(Vec::new())
    }

    pub fn key(&self, k: &str) -> Position {
        let mut v = self.0.clone();
        v.push(PosElem::Key(k.to_string()));
        Position(v)
    }

    pub fn idx(&self, i: usize) -> Position {
        let mut v = self.0.clone();
        v.push(PosElem::Index(i));
        Position(v)
    }

    pub fn to_json(&self) -> Json {
        Json::Array(
            self.0
                .iter()
                .map(|e| match e {
                    PosElem::Index(i) => json!(i),
                    PosElem::Key(k) => json!(k),
                })
                .collect(),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidationFailure {
    pub field: Vec<String>,
    pub code: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FaunaError {
    pub code: String,
    pub description: String,
    pub position: Position,
    pub cause: Vec<FaunaError>,
    pub failures: Vec<ValidationFailure>,
    /// Internal classification used for the HTTP status code.
    pub kind: ErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Parse,
    InvalidArgument,
    PermissionDenied,
    NotFound,
    Other,
}

pub type Errors = Vec<FaunaError>;
pub type R<T> = Result<T, Errors>;

fn expected_list(expected: &[Type], provided: Type) -> String {
    match expected {
        [] => format!("{} provided.", provided),
        [t] => format!("{} expected, {} provided.", t, provided),
        [a, b] => format!("{} or {} expected, {} provided.", a, b, provided),
        ts => {
            let init: Vec<String> = ts[..ts.len() - 1].iter().map(|t| t.to_string()).collect();
            format!("{}, or {} expected, {} provided.", init.join(", "), ts[ts.len() - 1], provided)
        }
    }
}

impl FaunaError {
    pub fn new(code: &str, description: impl Into<String>, position: &Position, kind: ErrorKind) -> FaunaError {
        FaunaError {
            code: code.to_string(),
            description: description.into(),
            position: position.clone(),
            cause: Vec::new(),
            failures: Vec::new(),
            kind,
        }
    }

    // ---- parse errors ("invalid expression") ----

    pub fn invalid_json(msg: impl Into<String>) -> FaunaError {
        FaunaError::new("invalid expression", msg, &Position::root(), ErrorKind::Parse)
    }

    pub fn invalid_expr_type(expected: &[Type], provided: Type, pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", expected_list(expected, provided), pos, ErrorKind::Parse)
    }

    pub fn invalid_form(keys: &[&str], pos: &Position) -> FaunaError {
        FaunaError::new(
            "invalid expression",
            format!("No form/function found, or invalid argument keys: {{ {} }}.", keys.join(", ")),
            pos,
            ErrorKind::Parse,
        )
    }

    pub fn invalid_object_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid object.", pos, ErrorKind::Parse)
    }

    pub fn invalid_ref_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid ref.", pos, ErrorKind::Parse)
    }

    pub fn invalid_set_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid set.", pos, ErrorKind::Parse)
    }

    pub fn invalid_time_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid time.", pos, ErrorKind::Parse)
    }

    pub fn invalid_date_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid date.", pos, ErrorKind::Parse)
    }

    pub fn invalid_bytes_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid bytes.", pos, ErrorKind::Parse)
    }

    pub fn invalid_lambda_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Invalid lambda.", pos, ErrorKind::Parse)
    }

    pub fn empty_expr(pos: &Position) -> FaunaError {
        FaunaError::new("invalid expression", "Expression list cannot be empty.", pos, ErrorKind::Parse)
    }

    pub fn unbound_variable(name: &str, pos: &Position) -> FaunaError {
        FaunaError::new(
            "invalid expression",
            format!("Variable '{}' is not defined.", name),
            pos,
            ErrorKind::Parse,
        )
    }

    // ---- eval errors ----

    pub fn transaction_abort(description: impl Into<String>, pos: &Position) -> FaunaError {
        FaunaError::new("transaction aborted", description, pos, ErrorKind::Other)
    }

    pub fn invalid_argument(expected: &[Type], provided: Type, pos: &Position) -> FaunaError {
        FaunaError::new("invalid argument", expected_list(expected, provided), pos, ErrorKind::InvalidArgument)
    }

    /// Generic "invalid argument" with a custom description, for the
    /// many `InvalidArgumentError` subclasses.
    pub fn invalid_argument_msg(description: impl Into<String>, pos: &Position) -> FaunaError {
        FaunaError::new("invalid argument", description, pos, ErrorKind::InvalidArgument)
    }

    pub fn empty_array_argument(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg("Non-empty array expected.", pos)
    }

    pub fn empty_set_argument(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg("Non-empty set expected.", pos)
    }

    pub fn invalid_lambda_arity(expected: usize, provided: usize, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(
            format!("Lambda expects an array with {} elements. Array contains {}.", expected, provided),
            pos,
        )
    }

    pub fn bounds(argument: &str, predicate: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("{} must be {}", argument, predicate), pos)
    }

    pub fn invalid_regex(pattern: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(
            format!("Search pattern /{}/ is not a valid regular expression.", pattern),
            pos,
        )
    }

    pub fn invalid_cursor_object(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(
            "Invalid cursor object. Expected an object with either 'before' or 'after' key in it.",
            pos,
        )
    }

    pub fn invalid_time_argument(s: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("Cannot cast '{}' to a time.", s), pos)
    }

    pub fn invalid_time_unit(unit: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("Invalid time unit '{}'.", unit), pos)
    }

    pub fn invalid_date_argument(s: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("Cannot cast '{}' to a date.", s), pos)
    }

    pub fn divide_by_zero(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg("Illegal division by zero.", pos)
    }

    pub fn invalid_cast(from: Type, to: Type, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("Cannot cast {} to {}.", from, to), pos)
    }

    pub fn invalid_normalizer(provided: Type, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(
            format!("String 'NFKCCaseFold', 'NFC', 'NFD', 'NFKC', or 'NFKD' expected, {} provided.", provided),
            pos,
        )
    }

    pub fn invalid_conversion_format(conversion: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("Invalid conversion format '{}'.", conversion), pos)
    }

    pub fn invalid_date_time_suffix(suffix: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(format!("Invalid date/time suffix '{}'.", suffix), pos)
    }

    pub fn incompatible_time_arguments(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg("Incompatible time arguments.", pos)
    }

    pub fn non_numeric_sub_id(id: &str, pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg(
            format!("Number or numeric String expected, non-numeric '{}' provided.", id),
            pos,
        )
    }

    pub fn invalid_schema_class_argument(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg("Regular Class Ref expected, Schema Class Ref provided.", pos)
    }

    pub fn sets_not_compatible(pos: &Position) -> FaunaError {
        FaunaError::invalid_argument_msg("Sets are not compatible.", pos)
    }

    pub fn mixed_arguments(expected: &[Type], pos: &Position) -> FaunaError {
        let names: Vec<String> = expected.iter().map(|t| t.to_string()).collect();
        FaunaError::invalid_argument_msg(
            format!("Mixed argument types: expected all of {}.", names.join(", ")),
            pos,
        )
    }

    /// `UnresolvedRefError`: a `Collection("x")`, `Index("x")`, etc. whose
    /// name does not exist. `kind` is "collection", "index", "function",
    /// "role" or "database".
    pub fn unresolved_ref(kind: &str, name: &str, pos: &Position) -> FaunaError {
        FaunaError::new(
            "invalid ref",
            format!("Ref refers to undefined {} '{}'", kind, name),
            pos,
            ErrorKind::Other,
        )
    }

    pub fn invalid_native_class_ref(name: &str, pos: &Position) -> FaunaError {
        FaunaError::new(
            "invalid ref",
            format!("Valid native collection name expected, '{}' provided.", name),
            pos,
            ErrorKind::Other,
        )
    }

    pub fn missing_identity(pos: &Position) -> FaunaError {
        FaunaError::new(
            "missing identity",
            "Authentication does not contain an identity",
            pos,
            ErrorKind::Other,
        )
    }

    pub fn value_not_found(path: &[crate::eval::cast::PathElem], pos: &Position) -> FaunaError {
        let es: Vec<String> = path.iter().map(|e| e.to_string()).collect();
        FaunaError::new(
            "value not found",
            format!("Value not found at path [{}].", es.join(",")),
            pos,
            ErrorKind::NotFound,
        )
    }

    /// `InstanceNotFound` for a document ref.
    pub fn document_not_found(pos: &Position) -> FaunaError {
        FaunaError::new("instance not found", "Document not found.", pos, ErrorKind::NotFound)
    }

    /// `InstanceNotFound` for a native resource: `kind` is "Collection",
    /// "Index", "Function", "Role", "Database", "Key".
    pub fn native_not_found(kind: &str, pos: &Position) -> FaunaError {
        FaunaError::new("instance not found", format!("{} not found.", kind), pos, ErrorKind::NotFound)
    }

    pub fn set_not_found(pos: &Position) -> FaunaError {
        FaunaError::new("instance not found", "Set not found.", pos, ErrorKind::NotFound)
    }

    pub fn index_inactive(name: &str, pos: &Position) -> FaunaError {
        FaunaError::new(
            "instance not found",
            format!("Index '{}' is not active.", name),
            pos,
            ErrorKind::NotFound,
        )
    }

    pub fn document_already_exists(pos: &Position) -> FaunaError {
        FaunaError::new("instance already exists", "Document already exists.", pos, ErrorKind::Other)
    }

    pub fn native_already_exists(kind: &str, pos: &Position) -> FaunaError {
        FaunaError::new(
            "instance already exists",
            format!("{} already exists.", kind),
            pos,
            ErrorKind::Other,
        )
    }

    pub fn instance_not_unique(pos: &Position) -> FaunaError {
        FaunaError::new("instance not unique", "document is not unique.", pos, ErrorKind::Other)
    }

    pub fn validation_failed(failures: Vec<ValidationFailure>, pos: &Position) -> FaunaError {
        let mut e = FaunaError::new("validation failed", "document data is not valid.", pos, ErrorKind::Other);
        e.failures = failures;
        e
    }

    pub fn call_error(cause: Errors, pos: &Position) -> FaunaError {
        let mut e = FaunaError::new(
            "call error",
            "Calling the function resulted in an error.",
            pos,
            ErrorKind::Other,
        );
        e.cause = cause;
        e
    }

    pub fn stack_overflow(pos: &Position) -> FaunaError {
        FaunaError::new(
            "stack overflow",
            format!("Call stack reached the maximum limit of {}.", crate::eval::MAX_STACK_DEPTH),
            pos,
            ErrorKind::Other,
        )
    }

    pub fn permission_denied(pos: &Position) -> FaunaError {
        FaunaError::new(
            "permission denied",
            "Insufficient privileges to perform the action.",
            pos,
            ErrorKind::PermissionDenied,
        )
    }

    // ---- emulator-specific ----

    /// A feature the emulator deliberately does not implement. The code is
    /// not a real Fauna code so no test can accidentally pass by matching
    /// a Fauna error.
    pub fn not_emulated(feature: &str, pos: &Position) -> FaunaError {
        FaunaError::new(
            "not emulated",
            format!(
                "The FQL emulator does not implement {}. Run this test against the fauna/faunadb container instead.",
                feature
            ),
            pos,
            ErrorKind::Other,
        )
    }

    /// The fixture size cap. Deliberately loud.
    pub fn emulator_limit(what: &str, limit: usize, pos: &Position) -> FaunaError {
        FaunaError::new(
            "emulator limit exceeded",
            format!(
                "The FQL emulator is for small test fixtures and is not a database: {} exceeded the cap of {}. Do not benchmark it; seed less data or use the fauna/faunadb container.",
                what, limit
            ),
            pos,
            ErrorKind::Other,
        )
    }

    pub fn to_json(&self) -> Json {
        let mut o = serde_json::Map::new();
        o.insert("code".into(), json!(self.code));
        o.insert("description".into(), json!(self.description));
        o.insert("position".into(), self.position.to_json());
        if !self.failures.is_empty() {
            o.insert(
                "failures".into(),
                Json::Array(
                    self.failures
                        .iter()
                        .map(|f| json!({"field": f.field, "code": f.code, "description": f.description}))
                        .collect(),
                ),
            );
        }
        if !self.cause.is_empty() {
            o.insert("cause".into(), Json::Array(self.cause.iter().map(|c| c.to_json()).collect()));
        }
        Json::Object(o)
    }
}

/// HTTP status for a list of errors, per `Error.statusCode`.
pub fn status_code(errs: &[FaunaError]) -> u16 {
    let rank = |e: &FaunaError| match e.kind {
        ErrorKind::Parse => 3,
        ErrorKind::InvalidArgument => 2,
        ErrorKind::PermissionDenied => 1,
        _ => 0,
    };
    let max = errs.iter().max_by_key(|e| rank(e));
    match max.map(|e| e.kind) {
        Some(ErrorKind::NotFound) => 404,
        Some(ErrorKind::PermissionDenied) => 403,
        _ => 400,
    }
}

pub fn one<T>(e: FaunaError) -> R<T> {
    Err(vec![e])
}
