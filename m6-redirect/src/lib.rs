//! `m6-redirect`: a generic static redirect engine in the Cloudflare Bulk
//! Redirects shape.
//!
//! It exact-matches host and path against a list of rules, answers a redirect
//! with the rule's status and target, and answers 404 otherwise. It ignores
//! the query. The list lives in one JSON file, read at start and on the
//! backend reload signal, and is replaced over the internal listener with a
//! `POST /m6-redirect/rules` carrying the Cloudflare list-items JSON.

#![deny(unsafe_code)]

pub mod config;
pub mod rules;
