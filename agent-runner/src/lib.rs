//! `remoter-agent` — the daemon that polls the Remoter backend for tickets
//! assigned to its agent user, claims them, and drives a dev-agent through the
//! plan → plan-review → implement → review workflow (docs/specs/remoter-agent.md).
//!
//! The daemon talks to the backend only (REST + the WebSocket event stream,
//! spec §4.5): it holds no DB credentials and every state change goes through
//! the backend API, so the human-acceptance gates and claim guards are always
//! enforced (spec §2.2).

pub mod claim;
pub mod client;
pub mod config;
pub mod container;
pub mod driver;
pub mod events;
pub mod forge;
pub mod image;
pub mod logs;
pub mod logstore;
pub mod logstream;
pub mod ports;
pub mod review;
pub mod run;
pub mod session_log;
pub mod staging;
pub mod supervise;
pub mod sync;
pub mod workspace;
