//! Reverse DNS, bounded, for checking a claim against its source.
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
//! # Why this is here and not in the serving path
//!
//! **`m6-http` must never do this.** A monitoring endpoint that performs a
//! network round trip is a monitoring endpoint that learns to hang, which is
//! why `/perf` reports URL backends by presence and never by reachability. A
//! monitor is the opposite case: it already makes network calls with a
//! deadline to poll every node, and going to look is the whole of its job.
//!
//! It lives in core because core is the only crate a service links.
//!
//! # Why a thread and a deadline
//!
//! `getnameinfo(3)` is blocking with no timeout parameter, and the resolver it
//! consults may be slow, unreachable, or controlled by the same person as the
//! address being looked up. An unbounded call in a monitoring run is a hostile
//! address away from stalling the run it was meant to report. So the call runs
//! on a worker thread and the caller waits with a deadline. A thread still
//! blocked at the deadline is abandoned rather than joined: it holds nothing
//! the caller needs, and the alternative is the stall this exists to prevent.
//!
//! No new dependency. `libc` is already in core.
//!
//! # A PTR record alone proves nothing
//!
//! Whoever controls an address's reverse zone can make it say anything,
//! including `crawl-1-2-3.example.com`. Every operator that documents the
//! method documents the second half too: resolve the name forward and check
//! that the original address is among the results. [`reverse_confirmed`] does
//! both and returns [`Ptr::Name`] only when the forward lookup contains the
//! address. A name whose forward lookup completes without the address is
//! [`Ptr::Unconfirmed`], which is a false claim. A forward lookup that fails
//! or times out is [`Ptr::Failed`], which is unverified.
//!
//! # Bounded threads
//!
//! At most [`MAX_LOOKUP_THREADS`] lookup threads are alive at once, counting
//! threads abandoned at their deadline and still inside the resolver. A lookup
//! that cannot get a slot reads [`Ptr::Failed`] without starting a thread, so
//! a resolver that never answers costs a fixed number of threads however many
//! addresses a run checks.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

/// The most lookup threads alive at once, abandoned ones included.
///
/// A monitoring run checks a handful of addresses, so eight leaves room for
/// every one of them to be in flight while capping what a resolver that never
/// answers can hold: eight blocked threads, then every later lookup reads
/// unverified at once.
pub const MAX_LOOKUP_THREADS: usize = 8;

/// A count of live lookup threads against a limit.
struct Slots {
    live: AtomicUsize,
    limit: usize,
}

/// One held slot, released when the thread that owns it finishes.
struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Slots {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Slots {
            live: AtomicUsize::new(0),
            limit,
        })
    }

    fn acquire(self: &Arc<Self>) -> Option<Slot> {
        // A compare-exchange loop, because `fetch_update` is renamed in newer
        // toolchains and the new name is not in the minimum supported one.
        let mut n = self.live.load(Ordering::Acquire);
        loop {
            if n >= self.limit {
                return None;
            }
            match self
                .live
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(Slot(Arc::clone(self))),
                Err(now) => n = now,
            }
        }
    }
}

fn global_slots() -> &'static Arc<Slots> {
    static SLOTS: OnceLock<Arc<Slots>> = OnceLock::new();
    SLOTS.get_or_init(|| Slots::new(MAX_LOOKUP_THREADS))
}

/// Run `f` on a worker thread and wait up to `timeout` for its result.
///
/// `None` means no result: no slot was free, the thread could not be started,
/// or the deadline passed. In the last case the thread is abandoned and keeps
/// its slot until it returns.
fn bounded<T: Send + 'static>(
    slots: &Arc<Slots>,
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let slot = slots.acquire()?;
    let (tx, rx) = mpsc::channel();
    // Detached deliberately: see the module header. The handle is dropped, so
    // a thread still inside the resolver at the deadline finishes on its own
    // and its send goes nowhere.
    std::thread::Builder::new()
        .name("m6-resolve".into())
        .spawn(move || {
            let _slot = slot;
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(timeout).ok()
}

/// The two resolver calls, so tests can stand in for a resolver.
trait Lookup: Send + Sync + 'static {
    /// The PTR name for an address.
    fn reverse(&self, ip: IpAddr) -> Ptr;
    /// The addresses a name resolves to.
    fn forward(&self, name: &str) -> Forward;
}

/// The result of resolving a name forward.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Forward {
    /// The lookup completed. An empty list means the name does not exist.
    Addrs(Vec<IpAddr>),
    /// The resolver could not answer.
    Failed,
}

/// The system resolver, through `getnameinfo` and `getaddrinfo`.
struct System;

impl Lookup for System {
    fn reverse(&self, ip: IpAddr) -> Ptr {
        blocking_reverse(ip)
    }
    fn forward(&self, name: &str) -> Forward {
        blocking_forward(name)
    }
}

/// The result of asking what name an address has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ptr {
    /// A name was returned. From [`reverse_confirmed`] it is also
    /// forward-confirmed: resolving it returns the address.
    Name(String),
    /// A PTR name was found and the forward lookup of that name completed
    /// without returning the address. Whoever controls the reverse zone can
    /// publish any name, so this is a false claim.
    Unconfirmed(String),
    /// The lookup completed and the address has no PTR record. Unverified,
    /// which is not the same as false.
    None,
    /// The lookup did not complete in time, or the resolver refused. Says
    /// nothing about the address.
    Failed,
}

impl Ptr {
    /// The name, if there is one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Ptr::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Whether this name supports a claim to be `domain`.
    ///
    /// A suffix match on a dot boundary, so `bot.example.com` matches
    /// `example.com` and `example.com.attacker.net` does not. The trailing dot
    /// a resolver may return is ignored. Case-insensitive, because DNS is.
    ///
    /// This is the check every crawler operator documents, and the dot
    /// boundary is the whole of it: `notexample.com` ends with `example.com`
    /// as a string and is a different domain.
    pub fn verifies(&self, domain: &str) -> bool {
        let Ptr::Name(name) = self else {
            return false;
        };
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        if domain.is_empty() {
            return false;
        }
        name == domain || name.ends_with(&format!(".{domain}"))
    }
}

/// Default deadline for one lookup.
///
/// Short on purpose. This runs once per distinct address in a monitoring run,
/// and a resolver that cannot answer in two seconds is one whose answer is not
/// worth delaying a report for. The outcome of waiting longer is the same
/// report, later.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Look up the PTR name for an address, giving up after `timeout`.
///
/// This is the reverse half only and does not confirm the name. Use
/// [`reverse_confirmed`] to check a claim.
///
/// `addr` is parsed rather than passed through: `getnameinfo` is being asked
/// about a numeric address, so anything that is not one is a caller error and
/// is reported as [`Ptr::Failed`] rather than sent to a resolver.
pub fn reverse(addr: &str, timeout: Duration) -> Ptr {
    let Ok(ip) = addr.parse::<IpAddr>() else {
        return Ptr::Failed;
    };
    bounded(global_slots(), timeout, move || blocking_reverse(ip)).unwrap_or(Ptr::Failed)
}

/// Look up the PTR name for an address and confirm it forward.
///
/// One deadline covers both lookups. The result is [`Ptr::Name`] only when
/// resolving the PTR name returns `addr`, [`Ptr::Unconfirmed`] when that
/// lookup completes without it, and [`Ptr::Failed`] when either lookup fails,
/// times out or finds no free thread slot.
pub fn reverse_confirmed(addr: &str, timeout: Duration) -> Ptr {
    let Ok(ip) = addr.parse::<IpAddr>() else {
        return Ptr::Failed;
    };
    confirm(ip, timeout, Arc::new(System), global_slots())
}

fn confirm(ip: IpAddr, timeout: Duration, lookup: Arc<dyn Lookup>, slots: &Arc<Slots>) -> Ptr {
    let started = Instant::now();
    let rev = Arc::clone(&lookup);
    let name = match bounded(slots, timeout, move || rev.reverse(ip)) {
        Some(Ptr::Name(n)) => n,
        Some(other) => return other,
        None => return Ptr::Failed,
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    let target = name.clone();
    match bounded(slots, remaining, move || lookup.forward(&target)) {
        Some(Forward::Addrs(addrs)) => {
            let want = ip.to_canonical();
            if addrs.iter().any(|a| a.to_canonical() == want) {
                Ptr::Name(name)
            } else {
                Ptr::Unconfirmed(name)
            }
        }
        Some(Forward::Failed) | None => Ptr::Failed,
    }
}

/// Look up the PTR name for an address with the default deadline.
pub fn reverse_default(addr: &str) -> Ptr {
    reverse(addr, DEFAULT_TIMEOUT)
}

/// [`reverse_confirmed`] with the default deadline.
pub fn reverse_confirmed_default(addr: &str) -> Ptr {
    reverse_confirmed(addr, DEFAULT_TIMEOUT)
}

/// The blocking call itself.
///
/// The sockaddr is a stack local in each arm and `getnameinfo` is called while
/// it is still in scope, so there is no allocation to free and no pointer that
/// outlives what it points at. An earlier shape built the sockaddr, returned a
/// pointer to it, and called afterwards, which is exactly how FFI comes to
/// read freed memory.
fn blocking_reverse(ip: IpAddr) -> Ptr {
    // NI_MAXHOST.
    let mut host = [0 as libc::c_char; 1025];
    // NI_NAMEREQD makes "no PTR record" an error rather than the numeric
    // address echoed back, which is the distinction this module exists for.
    // NI_NUMERICSERV stops the service half being looked up at all.
    let flags = libc::NI_NAMEREQD | libc::NI_NUMERICSERV;

    let rc = match ip {
        IpAddr::V4(v4) => {
            let mut raw: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            // `octets()` is network order and `s_addr` is stored in network
            // order, so the bytes go across unchanged.
            raw.sin_addr.s_addr = u32::from_ne_bytes(v4.octets());
            // SAFETY: `raw` is a fully initialised sockaddr_in alive for this
            // call, its exact size is passed as salen, and `host` is bounded
            // by its own length.
            unsafe {
                libc::getnameinfo(
                    &raw as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    host.as_mut_ptr(),
                    host.len() as libc::socklen_t,
                    std::ptr::null_mut(),
                    0,
                    flags,
                )
            }
        }
        IpAddr::V6(v6) => {
            let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_addr.s6_addr = v6.octets();
            // SAFETY: as above, for sockaddr_in6.
            unsafe {
                libc::getnameinfo(
                    &raw as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    host.as_mut_ptr(),
                    host.len() as libc::socklen_t,
                    std::ptr::null_mut(),
                    0,
                    flags,
                )
            }
        }
    };

    if rc != 0 {
        // EAI_NONAME is the expected "no PTR record" outcome under
        // NI_NAMEREQD. Everything else is a resolver that could not answer,
        // and the two are reported differently on purpose.
        return if rc == libc::EAI_NONAME {
            Ptr::None
        } else {
            Ptr::Failed
        };
    }

    // SAFETY: getnameinfo returned 0, so `host` holds a NUL-terminated string
    // within its own length.
    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) };
    match name.to_str() {
        Ok(s) if !s.is_empty() => Ptr::Name(s.to_string()),
        _ => Ptr::None,
    }
}

/// The blocking forward call.
///
/// Returns every address `name` resolves to. `EAI_NONAME` means the name
/// has no address, which is a completed lookup with an empty answer. Any other error means the resolver could not answer.
fn blocking_forward(name: &str) -> Forward {
    let Ok(cname) = std::ffi::CString::new(name) else {
        return Forward::Failed;
    };
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_family = libc::AF_UNSPEC;
    // One socket type, so each address comes back once rather than per type.
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut res: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `cname` is NUL-terminated and outlives the call, `hints` is
    // initialised, and `res` is a valid out-pointer.
    let rc = unsafe { libc::getaddrinfo(cname.as_ptr(), std::ptr::null(), &hints, &mut res) };
    if rc != 0 {
        return if rc == libc::EAI_NONAME {
            Forward::Addrs(Vec::new())
        } else {
            Forward::Failed
        };
    }
    let mut out = Vec::new();
    let mut cur = res;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list getaddrinfo returned, which
        // stays valid until freeaddrinfo below.
        let ai = unsafe { &*cur };
        if !ai.ai_addr.is_null() {
            if ai.ai_family == libc::AF_INET
                && ai.ai_addrlen as usize >= std::mem::size_of::<libc::sockaddr_in>()
            {
                // SAFETY: family and length checked; the node owns the bytes.
                let sa = unsafe { &*(ai.ai_addr as *const libc::sockaddr_in) };
                out.push(IpAddr::from(sa.sin_addr.s_addr.to_ne_bytes()));
            } else if ai.ai_family == libc::AF_INET6
                && ai.ai_addrlen as usize >= std::mem::size_of::<libc::sockaddr_in6>()
            {
                // SAFETY: as above, for sockaddr_in6.
                let sa = unsafe { &*(ai.ai_addr as *const libc::sockaddr_in6) };
                out.push(IpAddr::from(sa.sin6_addr.s6_addr));
            }
        }
        cur = ai.ai_next;
    }
    // SAFETY: `res` came from a successful getaddrinfo and is freed once.
    unsafe { libc::freeaddrinfo(res) };
    Forward::Addrs(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dot boundary is the whole of the check, so it gets the most tests.
    /// A suffix match without it accepts `example.com.attacker.net`, which is
    /// a domain the attacker controls and which ends with the string being
    /// looked for.
    #[test]
    fn verification_matches_on_a_dot_boundary_only() {
        let n = |s: &str| Ptr::Name(s.to_string());
        assert!(n("crawl-1-2-3.example.com").verifies("example.com"));
        assert!(n("example.com").verifies("example.com"));
        // The trailing dot a resolver may return.
        assert!(n("crawl.example.com.").verifies("example.com"));
        // DNS is case-insensitive.
        assert!(n("Crawl.Example.COM").verifies("example.com"));

        // The attack this exists to refuse.
        assert!(!n("example.com.attacker.net").verifies("example.com"));
        // A different domain that happens to end with the same letters.
        assert!(!n("notexample.com").verifies("example.com"));
        // A subdomain match must not run the other way.
        assert!(!n("example.com").verifies("crawl.example.com"));
    }

    /// An absent name and a failed lookup both fail verification, and neither
    /// is reported as the other.
    #[test]
    fn an_absent_name_and_a_failed_lookup_are_different_and_neither_verifies() {
        assert!(!Ptr::None.verifies("example.com"));
        assert!(!Ptr::Failed.verifies("example.com"));
        assert_ne!(Ptr::None, Ptr::Failed);
        assert_eq!(Ptr::None.name(), None);
        assert_eq!(Ptr::Failed.name(), None);
        assert_eq!(
            Ptr::Name("a.example.com".into()).name(),
            Some("a.example.com")
        );
    }

    /// An empty domain must never verify. A sighting whose expected domain is
    /// unknown would otherwise be reported as confirmed by any name at all.
    #[test]
    fn an_empty_domain_never_verifies() {
        assert!(!Ptr::Name("anything.example.com".into()).verifies(""));
        assert!(!Ptr::Name("anything.example.com".into()).verifies("."));
    }

    /// Anything that is not a numeric address is a caller error and never
    /// reaches a resolver.
    #[test]
    fn a_non_address_is_not_looked_up() {
        assert_eq!(reverse("not-an-address", DEFAULT_TIMEOUT), Ptr::Failed);
        assert_eq!(reverse("", DEFAULT_TIMEOUT), Ptr::Failed);
        assert_eq!(reverse("example.com", DEFAULT_TIMEOUT), Ptr::Failed);
    }

    /// The deadline is honoured rather than advisory. A zero timeout cannot
    /// wait for any resolver, so it must come back immediately and say it
    /// could not answer, which is the behaviour that keeps a hostile address
    /// from stalling a monitoring run.
    #[test]
    fn a_deadline_that_cannot_be_met_reports_failure_rather_than_waiting() {
        let started = std::time::Instant::now();
        let got = reverse("192.0.2.1", Duration::from_millis(0));
        assert_eq!(got, Ptr::Failed);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a zero deadline must not block"
        );
    }

    /// Loopback resolves on essentially every system, and the address is
    /// local so the test needs no network. Any of the three outcomes is
    /// acceptable because a machine may have no resolver at all; what is
    /// asserted is that the call returns, bounded, without panicking, for
    /// both address families.
    #[test]
    fn a_real_lookup_returns_within_its_deadline() {
        for addr in ["127.0.0.1", "::1"] {
            let started = std::time::Instant::now();
            let got = reverse(addr, Duration::from_secs(3));
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{addr} exceeded its deadline"
            );
            if let Ptr::Name(n) = &got {
                assert!(!n.is_empty(), "{addr} returned an empty name");
            }
        }
    }

    // -- Forward confirmation and the thread bound, with stub resolvers. ------

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A resolver that answers from tables, so no test touches a real one.
    struct Stub {
        ptr: HashMap<IpAddr, Ptr>,
        fwd: HashMap<String, Forward>,
    }

    impl Lookup for Stub {
        fn reverse(&self, ip: IpAddr) -> Ptr {
            self.ptr.get(&ip).cloned().unwrap_or(Ptr::None)
        }
        fn forward(&self, name: &str) -> Forward {
            self.fwd.get(name).cloned().unwrap_or(Forward::Failed)
        }
    }

    fn stub(ip: &str, ptr: Ptr, name: &str, fwd: Forward) -> Arc<Stub> {
        Arc::new(Stub {
            ptr: HashMap::from([(ip.parse().unwrap(), ptr)]),
            fwd: HashMap::from([(name.to_string(), fwd)]),
        })
    }

    const T: Duration = Duration::from_secs(2);

    fn run(ip: &str, lookup: Arc<dyn Lookup>) -> Ptr {
        confirm(
            ip.parse().unwrap(),
            T,
            lookup,
            &Slots::new(MAX_LOOKUP_THREADS),
        )
    }

    #[test]
    fn a_name_whose_forward_lookup_returns_the_address_is_confirmed() {
        let l = stub(
            "192.0.2.7",
            Ptr::Name("crawl.example.com".into()),
            "crawl.example.com",
            Forward::Addrs(vec![
                "192.0.2.1".parse().unwrap(),
                "192.0.2.7".parse().unwrap(),
            ]),
        );
        assert_eq!(run("192.0.2.7", l), Ptr::Name("crawl.example.com".into()));
    }

    #[test]
    fn an_ipv4_mapped_forward_answer_confirms_the_ipv4_address() {
        let l = stub(
            "192.0.2.7",
            Ptr::Name("crawl.example.com".into()),
            "crawl.example.com",
            Forward::Addrs(vec!["::ffff:192.0.2.7".parse().unwrap()]),
        );
        assert_eq!(run("192.0.2.7", l), Ptr::Name("crawl.example.com".into()));
    }

    /// The forged PTR: the name looks right and resolves to someone else.
    #[test]
    fn a_ptr_whose_forward_lookup_does_not_return_the_address_is_unconfirmed() {
        let l = stub(
            "192.0.2.7",
            Ptr::Name("crawl.example.com".into()),
            "crawl.example.com",
            Forward::Addrs(vec!["198.51.100.1".parse().unwrap()]),
        );
        let got = run("192.0.2.7", l);
        assert_eq!(got, Ptr::Unconfirmed("crawl.example.com".into()));
        assert!(
            !got.verifies("example.com"),
            "a forged name must not verify"
        );
    }

    #[test]
    fn a_name_that_does_not_resolve_forward_is_unconfirmed() {
        let l = stub(
            "192.0.2.7",
            Ptr::Name("gone.example.com".into()),
            "gone.example.com",
            Forward::Addrs(vec![]),
        );
        assert_eq!(
            run("192.0.2.7", l),
            Ptr::Unconfirmed("gone.example.com".into())
        );
    }

    #[test]
    fn a_failed_forward_lookup_is_unverified_not_false() {
        let l = stub(
            "192.0.2.7",
            Ptr::Name("crawl.example.com".into()),
            "crawl.example.com",
            Forward::Failed,
        );
        assert_eq!(run("192.0.2.7", l), Ptr::Failed);
    }

    #[test]
    fn no_ptr_is_unverified_and_makes_no_forward_lookup() {
        // The stub's forward table is empty, so a forward call would be Failed.
        let l = Arc::new(Stub {
            ptr: HashMap::new(),
            fwd: HashMap::new(),
        });
        assert_eq!(run("192.0.2.7", l), Ptr::None);
    }

    /// A resolver that blocks until released, counting how often it is called.
    struct Hang {
        calls: AtomicUsize,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl Lookup for Hang {
        fn reverse(&self, _ip: IpAddr) -> Ptr {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Returns when the sender is dropped or sends.
            let _ = self.release.lock().unwrap().recv();
            Ptr::None
        }
        fn forward(&self, _name: &str) -> Forward {
            Forward::Failed
        }
    }

    #[test]
    fn a_resolver_that_never_answers_reads_unverified_at_the_deadline() {
        let (tx, rx) = mpsc::channel::<()>();
        let hang = Arc::new(Hang {
            calls: AtomicUsize::new(0),
            release: Mutex::new(rx),
        });
        let started = Instant::now();
        let got = confirm(
            "192.0.2.7".parse().unwrap(),
            Duration::from_millis(100),
            hang,
            &Slots::new(MAX_LOOKUP_THREADS),
        );
        assert_eq!(got, Ptr::Failed);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(tx);
    }

    /// The bound: with every slot held by a lookup that never returns, the next
    /// lookups read unverified at once and start no thread, and a slot comes
    /// back when its thread finishes.
    #[test]
    fn the_thread_bound_holds_with_a_resolver_that_never_returns() {
        let slots = Slots::new(MAX_LOOKUP_THREADS);
        let (tx, rx) = mpsc::channel::<()>();
        let hang = Arc::new(Hang {
            calls: AtomicUsize::new(0),
            release: Mutex::new(rx),
        });
        // Only one blocked thread can hold the receiver lock, but every one of
        // them is alive and holds a slot, which is what the bound counts.
        for _ in 0..MAX_LOOKUP_THREADS {
            let got = confirm(
                "192.0.2.7".parse().unwrap(),
                Duration::from_millis(20),
                hang.clone(),
                &slots,
            );
            assert_eq!(got, Ptr::Failed);
        }
        assert_eq!(slots.live.load(Ordering::SeqCst), MAX_LOOKUP_THREADS);

        let started = Instant::now();
        for _ in 0..50 {
            let got = confirm(
                "192.0.2.7".parse().unwrap(),
                Duration::from_secs(5),
                hang.clone(),
                &slots,
            );
            assert_eq!(got, Ptr::Failed);
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "lookups past the bound must not wait"
        );
        assert_eq!(slots.live.load(Ordering::SeqCst), MAX_LOOKUP_THREADS);
        assert!(
            hang.calls.load(Ordering::SeqCst) <= MAX_LOOKUP_THREADS,
            "no thread may start past the bound"
        );

        // Release the resolver: the abandoned threads finish and free slots.
        drop(tx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while slots.live.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(slots.live.load(Ordering::SeqCst), 0);
        let l = stub("192.0.2.7", Ptr::None, "x", Forward::Failed);
        assert_eq!(
            confirm("192.0.2.7".parse().unwrap(), T, l, &slots),
            Ptr::None
        );
    }

    /// The system resolver path: loopback needs no network, and every outcome
    /// is acceptable. What is asserted is that the call returns bounded and
    /// that a name is only ever returned forward-confirmed.
    #[test]
    fn the_system_forward_lookup_of_localhost_returns_loopback() {
        match blocking_forward("localhost") {
            Forward::Addrs(a) => assert!(
                a.is_empty() || a.iter().any(|x| x.is_loopback()),
                "localhost resolved to {a:?}"
            ),
            Forward::Failed => {}
        }
        assert!(matches!(
            reverse_confirmed("not-an-address", T),
            Ptr::Failed
        ));
    }
}
