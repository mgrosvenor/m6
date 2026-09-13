//! The error type handlers return, and the status it becomes.
//!
//! One type, so that the mapping from a failure to an HTTP status lives in one
//! place instead of at each call site. `NotFound` is 404, `BadRequest` is 400,
//! and `Other` is 500 with the detail logged rather than sent.
//!
//! The distinction that matters: a path parameter containing `..` is a
//! `NotFound`, not a `BadRequest`. Answering 400 confirms to the sender that
//! their traversal was recognised as traversal, which tells them their payload
//! reached the router and is worth varying.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,

    #[error("forbidden")]
    Forbidden,

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Library-level result type.
pub type Result<T> = std::result::Result<T, Error>;
