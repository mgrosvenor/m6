//! The credential decision, in one place.
//!
//! The edge will ask the authority and then apply a route's `require` to the
//! answer. The types on both sides of that exchange live here: the identity a
//! `/check` 200 carries, the `require` matcher, the answer parser, the decision
//! the edge acts on, and the one encoder for `X-Auth-Claims`.
//!
//! No I/O. The service owns the transport: `m6-auth-server` answers the route,
//! and `m6-http` will send the check over its socket-backend dispatch. Nothing
//! here caches a decision: an answer is used once, for one request.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::http::TokenRefusal;

/// The identity a `/check` 200 answers: who, and what the database says.
///
/// Roles and groups are vectors rather than options because the authority
/// always answers both lists, possibly empty. A 200 that omits a field is not
/// an identity and parses as [`Answer::NoDecision`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub sub: String,
    pub username: String,
    pub roles: Vec<String>,
    pub groups: Vec<String>,
}

/// A `require` string: the list it reads and the name that must be in it.
///
/// The one definition of the format, read by the request-path check and by
/// config validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Require<'a> {
    Role(&'a str),
    Group(&'a str),
}

impl<'a> Require<'a> {
    /// Parse `"role:<name>"` or `"group:<name>"`. `None` for any other form.
    pub fn parse(require: &'a str) -> Option<Self> {
        if let Some(group) = require.strip_prefix("group:") {
            return Some(Self::Group(group));
        }
        require.strip_prefix("role:").map(Self::Role)
    }
}

/// Whether an identity satisfies the `require` string.
///
/// Format: `group:<name>` or `role:<name>`. Any other form denies.
pub fn check_require(identity: &Identity, require: &str) -> bool {
    let (list, name) = match Require::parse(require) {
        Some(Require::Group(group)) => (&identity.groups, group),
        Some(Require::Role(role)) => (&identity.roles, role),
        None => return false,
    };
    list.iter().any(|value| value == name)
}

/// What a `/check` answer said, before the route's `require` is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A 200 with a complete identity.
    Identified(Identity),
    /// A 401: the authority judged the credential and refused it.
    Refused,
    /// No decision: any other status, or a 200 that does not parse. The edge
    /// answers 503 for this, never a 401.
    NoDecision,
}

impl Answer {
    /// Apply one route's `require` to this answer.
    ///
    /// `Identified` that satisfies the requirement is `Allow`, one that does
    /// not is `Deny`, `Refused` is `NoCredential`, and `NoDecision` stays
    /// `NoDecision` so the caller answers 503 rather than logging a valid user
    /// out with a 401.
    pub fn decide(self, require: &str) -> Decision {
        match self {
            Answer::Identified(identity) if check_require(&identity, require) => {
                Decision::Allow(identity)
            }
            Answer::Identified(_) => Decision::Deny,
            Answer::Refused => Decision::NoCredential,
            Answer::NoDecision => Decision::NoDecision,
        }
    }
}

/// Parse a `/check` answer.
///
/// The rule is total. A 200 with the four identity fields is `Identified`, a
/// 401 is `Refused`, and every other status and every unparsable 200 is
/// `NoDecision`. A 403 is `NoDecision`: the authority answers 401 for a
/// credential it judged, and a 403 from it would be a route decision this edge
/// has not made.
pub fn parse_check(status: u16, body: &[u8]) -> Answer {
    match status {
        200 => serde_json::from_slice::<Identity>(body)
            .map(Answer::Identified)
            .unwrap_or(Answer::NoDecision),
        401 => Answer::Refused,
        _ => Answer::NoDecision,
    }
}

/// What the one token check decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The credential identifies this user and carries the route's
    /// requirement.
    Allow(Identity),
    /// No credential, or one that does not verify. A 401.
    NoCredential,
    /// The credential identifies a user who lacks the required role or group.
    /// A 403.
    Deny,
    /// The authority could not decide. A 503, never a 401.
    NoDecision,
}

impl Decision {
    /// The token-refusal bytes this decision is, or `None` when it is not a
    /// 401 or 403.
    ///
    /// `NoDecision` also returns `None`, and its answer is the 503 path
    /// (`backend_unavailable` in m6-http), not this refusal. A caller must
    /// match `NoDecision` before reading a `None` as permission: treating every
    /// `None` as allow would fail open on an unreachable authority.
    pub fn refusal(&self) -> Option<TokenRefusal> {
        match self {
            Decision::Allow(_) | Decision::NoDecision => None,
            Decision::NoCredential => Some(crate::http::token_refusal(false)),
            Decision::Deny => Some(crate::http::token_refusal(true)),
        }
    }
}

/// Encode an identity as base64 JSON for the `X-Auth-Claims` header.
///
/// The four keys here are the four `request::parse_auth_claims` reads. A token's
/// `exp`, `iss` and extra claims do not appear, because the edge forwards the
/// identity the authority answered and not the token it decoded before.
pub fn encode_claims_header(identity: &Identity) -> String {
    let json = serde_json::to_string(identity).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(roles: &[&str], groups: &[&str]) -> Identity {
        Identity {
            sub: "user-1".to_string(),
            username: "alice".to_string(),
            roles: roles.iter().map(|role| role.to_string()).collect(),
            groups: groups.iter().map(|group| group.to_string()).collect(),
        }
    }

    #[test]
    fn a_role_in_the_answer_satisfies_a_role_require() {
        let answer = Answer::Identified(identity(&["admin"], &[]));
        assert_eq!(
            answer.decide("role:admin"),
            Decision::Allow(identity(&["admin"], &[]))
        );
    }

    #[test]
    fn a_group_in_the_answer_satisfies_a_group_require() {
        let answer = Answer::Identified(identity(&[], &["editors"]));
        assert_eq!(
            answer.decide("group:editors"),
            Decision::Allow(identity(&[], &["editors"]))
        );
    }

    #[test]
    fn a_missing_role_is_deny() {
        let answer = Answer::Identified(identity(&["user"], &[]));
        assert_eq!(answer.decide("role:admin"), Decision::Deny);
    }

    #[test]
    fn an_empty_role_list_with_a_require_is_deny() {
        let answer = Answer::Identified(identity(&[], &[]));
        assert_eq!(answer.decide("role:admin"), Decision::Deny);
    }

    /// A 401 is the one status that becomes `NoCredential`.
    #[test]
    fn a_401_is_no_credential() {
        assert_eq!(
            parse_check(401, b"").decide("role:admin"),
            Decision::NoCredential
        );
    }

    /// **The wrong-decision test.** A 503 that became a 401 would tell a user
    /// with a valid credential to log in again. The authority did not refuse
    /// the credential, so the edge must answer 503 instead.
    #[test]
    fn no_decision_is_labelled_as_no_credential() {
        let answer = parse_check(503, b"");
        assert_eq!(answer, Answer::NoDecision);
        let decision = answer.decide("role:admin");
        assert_eq!(decision, Decision::NoDecision);
        assert_ne!(decision, Decision::NoCredential);
    }

    #[test]
    fn a_500_is_no_decision() {
        assert_eq!(parse_check(500, b""), Answer::NoDecision);
    }

    #[test]
    fn a_403_is_no_decision_and_not_a_credential_refusal() {
        assert_eq!(parse_check(403, b""), Answer::NoDecision);
        assert_ne!(parse_check(403, b""), Answer::Refused);
    }

    #[test]
    fn a_200_with_a_broken_body_is_no_decision() {
        assert_eq!(parse_check(200, b"not json"), Answer::NoDecision);
        assert_eq!(
            parse_check(200, br#"{"sub":"user-1"}"#),
            Answer::NoDecision,
            "an identity missing its role and group lists is not an answer"
        );
    }

    #[test]
    fn a_200_identity_parses() {
        let json = br#"{"sub":"user-1","username":"alice","roles":["admin"],"groups":["editors"]}"#;
        assert_eq!(
            parse_check(200, json),
            Answer::Identified(identity(&["admin"], &["editors"]))
        );
    }

    #[test]
    fn the_claims_header_carries_only_the_identity() {
        let encoded = encode_claims_header(&identity(&["admin"], &["editors"]));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .expect("base64");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("json");
        assert_eq!(json["sub"], "user-1");
        assert_eq!(json["username"], "alice");
        assert_eq!(json["roles"], serde_json::json!(["admin"]));
        assert_eq!(json["groups"], serde_json::json!(["editors"]));
        assert!(
            json.get("exp").is_none() && json.get("iss").is_none(),
            "the identity header must not carry token claims: {json}"
        );
    }

    #[test]
    fn a_decision_refusal_is_the_one_token_refusal() {
        assert_eq!(
            Decision::NoCredential.refusal().map(|refusal| refusal.0),
            Some(401)
        );
        assert_eq!(Decision::Deny.refusal().map(|refusal| refusal.0), Some(403));
        assert_eq!(Decision::Allow(identity(&[], &[])).refusal(), None);
        assert_eq!(Decision::NoDecision.refusal(), None);
    }
}
