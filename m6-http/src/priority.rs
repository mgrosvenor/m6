//! RFC 9218 Extensible Priorities: the client's `priority` request header.
//!
//! m6 parses the two parameters it acts on itself. Quiche's parser for the
//! same field needs its `sfv` feature, and enabling that would add a
//! dependency for a dictionary of two members. A server may ignore the field
//! entirely (RFC 9218 section 10), so this parser only has to be right about
//! the two parameters it reads and safe about everything else.

/// The urgency and incremental parameters of one `priority` field value
/// (RFC 9218 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Priority {
    /// The precedence of the response: 0 is the most urgent, 7 the least.
    /// The default is 3 (RFC 9218 section 4.1).
    pub urgency: u8,
    /// Whether the client can use the response as its bytes arrive. The
    /// default is false (RFC 9218 section 4.2).
    pub incremental: bool,
}

impl Default for Priority {
    fn default() -> Self {
        Priority {
            urgency: 3,
            incremental: false,
        }
    }
}

/// Parse a `priority` field value (RFC 9218 section 4).
///
/// The field is a Structured Fields Dictionary (RFC 8941 section 3.2), and
/// this reads the two members RFC 9218 defines from it. A member is a
/// Dictionary key, then optionally a value and parameters: `u` takes an
/// Integer from 0 to 7 (leading zeros and `-0` included), `i` takes a Boolean
/// or no value at all, and a member's parameters are ignored. Unknown members
/// are ignored (RFC 9218 section 4), and a `u` or `i` whose value has the
/// wrong type leaves its parameter at the default. A member whose key is not
/// a Dictionary key means the field is not a Dictionary at all, and the whole
/// field gives the default. Nothing is allocated.
pub fn parse(field: &[u8]) -> Priority {
    let mut priority = Priority::default();
    for member in field.split(|&byte| byte == b',') {
        let member = trim_ows(member);
        // A member is `key`, `key=value`, or either followed by parameters.
        // The parameters qualify the member and are ignored here.
        let (name, rest) = match member.iter().position(|&byte| byte == b'=' || byte == b';') {
            Some(at) => (&member[..at], Some(&member[at..])),
            None => (member, None),
        };
        if !is_key(name) {
            return Priority::default();
        }
        let value = rest.and_then(|rest| {
            let value = rest.strip_prefix(b"=")?;
            Some(match value.iter().position(|&byte| byte == b';') {
                Some(at) => &value[..at],
                None => value,
            })
        });
        match name {
            b"u" => {
                if let Some(urgency) = value.and_then(urgency_of) {
                    priority.urgency = urgency;
                }
            }
            // A member with no value is the Boolean true (RFC 8941 3.2).
            b"i" => {
                if let Some(incremental) = value.map_or(Some(true), incremental_of) {
                    priority.incremental = incremental;
                }
            }
            _ => {}
        }
    }
    priority
}

/// The priority a request asked for, from its header list. A request with no
/// `priority` field gets the default, as does one whose field does not parse.
pub fn from_headers<'a>(headers: impl Iterator<Item = (&'a [u8], &'a [u8])>) -> Priority {
    headers
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"priority"))
        .map_or_else(Priority::default, |(_, value)| parse(value))
}

/// Strip leading and trailing optional whitespace (RFC 9110 5.6.3).
fn trim_ows(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&byte| byte != b' ' && byte != b'\t')
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&byte| byte != b' ' && byte != b'\t')
        .map_or(start, |at| at + 1);
    &bytes[start..end]
}

/// An RFC 8941 dictionary key (section 3.2): a lowercase letter or `*`, then
/// lowercase letters, digits, `_`, `-`, `.` and `*`.
fn is_key(name: &[u8]) -> bool {
    let Some((first, rest)) = name.split_first() else {
        return false;
    };
    (first.is_ascii_lowercase() || *first == b'*')
        && rest.iter().all(|&byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b'*')
        })
}

/// `u` as RFC 9218 section 4.1 defines it: an RFC 8941 Integer from 0 to 7.
/// A value outside the range, or one that is not an Integer, is ignored. RFC
/// 8941 section 3.3.1 allows leading zeros and a signed zero, so `07` is 7
/// and `-0` is 0.
fn urgency_of(value: &[u8]) -> Option<u8> {
    let (negative, digits) = match value.strip_prefix(b"-") {
        Some(digits) => (true, digits),
        None => (false, value),
    };
    if digits.is_empty() || digits.len() > 15 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut magnitude = 0i64;
    for &digit in digits {
        magnitude = magnitude * 10 + i64::from(digit - b'0');
    }
    let value = if negative { -magnitude } else { magnitude };
    u8::try_from(value).ok().filter(|urgency| *urgency <= 7)
}

/// `i` as RFC 9218 section 4.2 defines it: a Boolean. Anything else is
/// ignored.
fn incremental_of(value: &[u8]) -> Option<bool> {
    match value {
        b"?1" => Some(true),
        b"?0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples RFC 9218 section 4 uses, plus the legal Integer forms,
    /// parameters, the out-of-range form and values that are not a Dictionary
    /// at all. A `u` outside 0 to 7 is ignored and leaves `i` alone; a field
    /// that is not a Dictionary gives both defaults.
    #[test]
    fn parses_rfc_9218_examples() {
        assert_eq!(
            parse(b"u=5, i"),
            Priority {
                urgency: 5,
                incremental: true
            }
        );
        assert_eq!(
            parse(b"u=0"),
            Priority {
                urgency: 0,
                incremental: false
            }
        );
        assert_eq!(
            parse(b"i=?0, u=2"),
            Priority {
                urgency: 2,
                incremental: false
            }
        );
        assert_eq!(
            parse(b"i, u=9"),
            Priority {
                urgency: 3,
                incremental: true
            },
            "9 is outside 0 to 7, so u is ignored and i is unchanged"
        );
        assert_eq!(parse(b"garbage"), Priority::default());
        assert_eq!(parse(b""), Priority::default());

        // RFC 8941 section 3.3.1: leading zeros and signed zero are Integers.
        assert_eq!(
            parse(b"u=07"),
            Priority {
                urgency: 7,
                incremental: false
            }
        );
        assert_eq!(
            parse(b"u=-0"),
            Priority {
                urgency: 0,
                incremental: false
            }
        );
        // A member's parameters qualify it and do not change its value.
        assert_eq!(
            parse(b"u=5;a"),
            Priority {
                urgency: 5,
                incremental: false
            }
        );
        assert_eq!(
            parse(b"i;x"),
            Priority {
                urgency: 3,
                incremental: true
            }
        );
        // A key that is not an RFC 8941 Dictionary key makes the whole field
        // foreign, so nothing in it is honoured.
        assert_eq!(parse(b"u=5, !x"), Priority::default());
        assert_eq!(parse(b"u=5, 9x"), Priority::default());
    }
}
