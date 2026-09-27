//! Everything Luthier decides about a package.
//!
//! The crate is one library behind two binaries and, later, a GUI: `api`
//! is the surface they call, and every other module is a stage of the same
//! pipeline. A request becomes a plan in [`resolver`], bytes in [`download`],
//! a vetted tree in [`archive`], files in [`install`], and a record in
//! [`state`]; [`registry`] is where manifests come from and [`layout`] is
//! where everything lands. Nothing here parses a manifest — that is
//! `luthier-manifest` — and nothing here prints, prompts or reads an
//! argument.
//!
//! Two rules hold throughout. Paths come from an injected [`Layout`] rather
//! than from `$HOME`, which is what lets the suite run hermetically; and
//! nothing downloaded is trusted before it is verified, which is why
//! [`download`] cannot hand [`archive`] a file that failed its checksum.

#![forbid(unsafe_code)]

pub mod api;
pub mod archive;
pub mod config;
pub mod download;
pub mod engine;
pub mod envfile;
pub mod error;
pub mod fsutil;
pub mod install;
pub mod layout;
pub mod registry;
pub mod resolver;
pub mod scan;
pub mod state;

pub use error::{Error, ExitCode, Result};
pub use layout::Layout;

pub use api::Session;
pub use config::Config;
