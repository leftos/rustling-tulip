//! The summarizer service: the provider keys, the provider table, the HTTP
//! call, the call through an agent's own CLI, the prompt and the text shaped
//! into it, and the reply every provider's answer is read into.

pub(crate) mod cli;
pub(crate) mod http;
pub(crate) mod keys;
pub(crate) mod prompt;
pub(crate) mod providers;
pub(crate) mod reply;
