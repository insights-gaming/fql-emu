//! Key permissions a query runs under.
//!
//! The server decides what a query may do from the key (or token) that
//! sent it. Only the distinction the language layer needs is modelled:
//! whether writes, `Call` and listing the privileged native collections are
//! allowed. Storage backends report their mode through
//! [`Backend::mode`](crate::Backend::mode); the interpreter denies the
//! affected forms at the positions the server uses
//! ([`deny_if_read_only`](crate::eval::deny_if_read_only)).

/// What queries may do: the permissions of the key that sent them.
///
/// (In fql-emu-core the mode is chosen when forking, `Emulator::fork_as`: a
/// fresh emulator is always `ReadWrite`, and a read-only copy is a fork, so
/// the base that was seeded stays the one writable instance.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    ReadWrite,
    /// A server-readonly key (`ServerReadOnlyPermissions`): every write,
    /// every `Call`, and listing roles/keys/databases/access providers
    /// answer `permission denied` (403) where the server would. Reads,
    /// `NewId()` and `Now()` still work, and the instance is guaranteed
    /// never to change, so it can be shared by tests that only read.
    ReadOnly,
    /// `ReadOnly`, except that `Call` of a function whose body has no
    /// write effect is allowed (`call` forms inside the body are rated as
    /// reads, since each nested `Call` is checked at its own site). A
    /// function that writes is still denied at the `Call`. This has no
    /// server equivalent: it exists so a shared read-only instance can
    /// still exercise read-only UDFs. It is an emulator extension of the
    /// server-readonly key, not a server permission set.
    ReadOnlyPureCalls,
}

impl Mode {
    pub fn is_read_only(self) -> bool {
        self != Mode::ReadWrite
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ReadWrite => "read-write",
            Mode::ReadOnly => "read-only",
            Mode::ReadOnlyPureCalls => "read-only-pure-calls",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "read-write" => Some(Mode::ReadWrite),
            "read-only" => Some(Mode::ReadOnly),
            "read-only-pure-calls" => Some(Mode::ReadOnlyPureCalls),
            _ => None,
        }
    }
}
