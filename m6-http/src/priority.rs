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
/// The field is a Structured Fields Dictionary (RFC 8941 section 3.2). Where
/// it parses, RFC 9218 requires a receiver to ignore unknown parameters,
/// parameters with out-of-range values and values of the wrong type, and that
/// is what this does: each member sets its parameter only when it is valid.
/// A field that is not a Dictionary at all gives the default. Nothing is
/// allocated.
pub fn parse(field: &[u8]) -> Priority {
    let mut priority = Priority::default();
    if field.iter().all(u8::is_ascii_whitespace) {
        return priority;
    }
    for member in field.split(|&byte| byte == b',') {
        let member = trim_ows(member);
        let (name, value) = match member.iter().position(|&byte| byte == b'=') {
            Some(at) => (trim_ows(&member[..at]), Some(trim_ows(&member[at + 1..]))),
            None => (member, None),
        };
        if !is_key(name) {
            return Priority::default();
        }
        match name {
            b"u" => {
                if let Some(urgency) = value.and_then(urgency_of) {
                    priority.urgency = urgency;
                }
            }
            // A parameter with no value is the Boolean true (RFC 8941 3.2).
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

/// An RFC 8941 dictionary key: a lowercase token. Anything else means the
/// field did not parse as a Dictionary, so the whole field is ignored.
fn is_key(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|&byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// `u` as RFC 9218 section 4.1 defines it: an Integer from 0 to 7. A value
/// outside the range, or one that is not an Integer, is ignored.
fn urgency_of(value: &[u8]) -> Option<u8> {
    match value {
        [digit @ b'0'..=b'7'] => Some(digit - b'0'),
        _ => None,
    }
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

    /// The examples RFC 9218 section 4 uses, plus the out-of-range and
    /// unparsable forms a client can send. A `u` outside 0 to 7 is ignored
    /// and leaves `i` alone; a field that is not a Dictionary gives both
    /// defaults.
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
    }
}
