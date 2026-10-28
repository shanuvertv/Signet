//! Network protocol clients for Signet.
//!
//! POP3 is hand-written (see `pop3`); SMTP submission wraps `lettre`.
//!
//! The reference server is Dovecot as shipped by cPanel (see `docs/adr/0001-scope-and-stack.md`),
//! with Mailpit standing in for local development and CI.

pub mod error;
pub mod imap;
pub mod pop3;
pub mod smtp;
pub mod tls;

pub use error::{Error, Result};
