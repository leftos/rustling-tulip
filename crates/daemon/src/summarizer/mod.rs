//! The summarizer service: the provider keys, the provider table, the HTTP
//! call, the call through an agent's own CLI, and the reply every provider's
//! answer is read into.

pub(crate) mod cli;
pub(crate) mod http;
pub(crate) mod keys;
pub(crate) mod providers;
pub(crate) mod reply;
