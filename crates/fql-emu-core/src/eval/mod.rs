//! The emulator's interpreter: the storage-independent interpreter and
//! forms of [`fql_lang::eval`] run against the in-memory
//! [`Database`], plus the forms that need storage
//! (`sets`, `read`, `write`).
//!
//! Everything in `fql_lang::eval` is re-exported here, with `Ctx` and
//! `Registry` fixed to the emulator's database.

pub mod read;
pub mod sets;
pub mod write;

use crate::error::{Position, R};
use crate::store::Database;
use crate::value::{Lambda, Ref, SetExpr, Value};
use crate::Mode;
pub use fql_lang::effect;
pub use fql_lang::eval::*;
use fql_lang::Backend;
use std::sync::OnceLock;

/// The evaluation context over the emulator's database.
pub type Ctx<'a> = fql_lang::eval::Ctx<'a, Database>;

/// The form registry over the emulator's database.
pub type Registry = fql_lang::eval::Registry<Database>;

/// Every form the emulator knows: the language forms, then the set, read
/// and write forms.
pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut r = language_forms();
        sets::register(&mut r);
        read::register(&mut r);
        write::register(&mut r);
        r
    })
}

impl Backend for Database {
    fn mode(&self) -> Mode {
        self.mode
    }

    fn identity(&self) -> Option<&Ref> {
        self.identity.as_ref()
    }

    fn new_id(&mut self) -> String {
        Database::new_id(self)
    }

    /// Schema refs resolve by name against the committed schema, so a
    /// schema object deleted earlier in the transaction still resolves
    /// (`Get` reports it gone); a document ref is checked in full.
    fn exists_by_ref(&self, r: &Ref) -> bool {
        if r.is_collection_ref() {
            self.collections.contains_key(&r.id)
        } else if r.is_index_ref() {
            self.indexes.contains_key(&r.id)
        } else if r.is_function_ref() {
            self.functions.contains_key(&r.id)
        } else if r.is_role_ref() {
            self.roles.contains_key(&r.id)
        } else {
            Database::exists_by_ref(self, r)
        }
    }

    fn function_body(&self, name: &str) -> Option<&Lambda> {
        self.functions.get(name).map(|f| &f.body)
    }

    fn materialize(ctx: &mut Ctx<'_>, set: &SetExpr, pos: &Position) -> R<Vec<Value>> {
        sets::materialize(ctx, set, pos)
    }
}
