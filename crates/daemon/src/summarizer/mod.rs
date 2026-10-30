//! The summarizer service: the provider keys, the provider table, the HTTP
//! call, the call through an agent's own CLI, the prompt and the text shaped
//! into it, the reply every provider's answer is read into, and the service
//! over them: its settings, per-session queue, cache and spend ledger; the
//! turn pipeline that summarizes each finished turn into its session's
//! summary ring; and the working refresh that puts a provisional line there
//! while a watched session is still in its turn.

pub(crate) mod cache;
pub(crate) mod cli;
pub(crate) mod http;
pub(crate) mod keys;
pub(crate) mod ledger;
pub(crate) mod policy;
pub(crate) mod prompt;
pub(crate) mod providers;
pub(crate) mod queue;
pub(crate) mod refresh;
pub(crate) mod reply;
pub(crate) mod service;
pub(crate) mod turns;
