//! Sako's HTTP stack: the `Sako.serve()` server and the `fetch()` client.
//! Independent of V8; the runtime adapts it to JS.

pub mod client;
pub mod server;

pub use client::{FetchRequest, FetchResponse, Redirect, fetch, set_user_agent};
pub use server::{HttpReply, HttpRequest, ReplyTo, RequestSink, ServerHandle, listen};
