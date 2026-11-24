//! Reverse DNS, for checking a claim against its source.
//!
//! A `User-Agent` is an assertion. Any client can send any string, so a
//! request claiming to be a well-known crawler is evidence of nothing until
//! the address it came from is checked. Every major crawler operator publishes
//! the same verification method: resolve the address to a name, and see
//! whether the name belongs to who the string claims.
//!
//! # Three outcomes, and they are not two
//!
//! [`Ptr`] distinguishes a name that was found from a lookup that found
//! nothing and from a lookup that could not be completed. Collapsing the last
//! two into "not verified" would be a small lie in the expensive direction: an
//! address with no PTR record is unverified and may be perfectly honest, while
//! a resolver that timed out says nothing about the address at all.
//!
//! # The lookup is a descriptor, not a call
//!
//! [`crate::dns::Confirm`] is the whole rule: it asks the PTR question, then
//! asks the name it gets forward and requires the original address among the
//! answers. It owns one UDP socket and never waits, so a service's own loop
//! registers it, drives it when it is readable, and drops it at the deadline.
//! No thread and no blocking resolver call sits behind this module.
//!
//! # A PTR record alone proves nothing
//!
//! Whoever controls an address's reverse zone can make it say anything,
//! including `crawl-1-2-3.example.com`. Every operator that documents the
//! method documents the second half too: resolve the name forward and check
//! that the original address is among the results. [`crate::dns::Confirm`]
//! returns [`Ptr::Name`] only when the forward answer contains the address. A
//! name whose forward lookup completes without the address is
//! [`Ptr::Unconfirmed`], which is a false claim. A forward lookup that fails or
//! times out is [`Ptr::Failed`], which is unverified.
//!
//! # Where this lives
//!
//! In core because core is the only crate a service links. **`m6-http` must
//! never call it.** A monitoring path that performs a network round trip is a
//! monitoring path that learns to hang, which is why the metrics document
//! reports URL backends by presence and never by reachability. The one caller
//! is `m6-monitor`, whose own loop is a monitor's job: it already talks to
//! every node with a deadline, and going to look is the point of it.
//!
//! [`Ptr`] is re-exported here because callers name `resolve::Ptr` and this
//! page states the rule.

pub use crate::dns::Ptr;
