use crate::config::RouteConfig;

/// Route params: at most 2 captures (relpath or stem+filename).
/// Vec<(name, value)> is faster than HashMap for this count.
pub type Params = Vec<(String, String)>;

/// Result of attempting to match a URL path against a route.
#[derive(Debug, PartialEq)]
pub enum MatchResult {
    /// Route matched and all params are valid.
    Matched(Params),
    /// Route structure matched but a param failed validation (e.g. contains `..`).
    /// Spec says this should be 400.
    InvalidParam,
    /// Route did not match at all.
    NoMatch,
}

/// A compiled route with parsed pattern segments.
#[derive(Debug, Clone)]
pub struct Route {
    pub raw_path: String,
    pub raw_root: String,
    /// Parsed segments of the path pattern.
    pub segments: Vec<Segment>,
    /// Specificity score (higher = more specific).
    pub specificity: i64,
    /// Whether this route uses tail mode (offset-based partial reads, no-store).
    pub tail: bool,
}

#[derive(Debug, Clone)]
pub enum Segment {
    Literal(String),
    Param(String),
    /// {relpath} captures the rest of the path including slashes
    CatchAll(String),
}

impl Route {
    pub fn from_config(cfg: &RouteConfig) -> Self {
        let segments = parse_pattern(&cfg.path);
        let specificity = compute_specificity(&segments, &cfg.path);
        Route {
            raw_path: cfg.path.clone(),
            raw_root: cfg.root.clone(),
            segments,
            specificity,
            tail: cfg.tail.unwrap_or(false),
        }
    }

    /// Try to match a URL path against this route.
    /// Returns a `MatchResult` distinguishing full match, invalid param (400),
    /// and no-match (404).
    pub fn match_path(&self, url_path: &str) -> MatchResult {
        let mut params: Params = Vec::with_capacity(2);
        let url_path = url_path.trim_start_matches('/');
        match match_segments(&self.segments, url_path, &mut params) {
            SegmentMatch::Ok => MatchResult::Matched(params),
            SegmentMatch::InvalidParam => MatchResult::InvalidParam,
            SegmentMatch::NoMatch => MatchResult::NoMatch,
        }
    }

    /// Resolve a filesystem path given matched params and the site directory.
    pub fn resolve_fs_path(
        &self,
        params: &Params,
        site_dir: &std::path::Path,
    ) -> std::path::PathBuf {
        // Expand root template with params
        let mut root = self.raw_root.clone();
        for (k, v) in params {
            root = root.replace(&format!("{{{}}}", k), v);
        }
        // The remaining path (relpath or filename) is in params
        let rel = params
            .iter()
            .find(|(k, _)| k == "relpath")
            .or_else(|| params.iter().find(|(k, _)| k == "filename"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        if rel.is_empty() {
            site_dir.join(&root)
        } else {
            site_dir.join(&root).join(rel)
        }
    }
}

fn parse_pattern(pattern: &str) -> Vec<Segment> {
    let pattern = pattern.trim_start_matches('/');
    let mut segments = Vec::new();
    for part in pattern.split('/') {
        if part.starts_with('{') && part.ends_with('}') {
            let name = &part[1..part.len() - 1];
            if name == "relpath" {
                segments.push(Segment::CatchAll(name.to_string()));
            } else {
                segments.push(Segment::Param(name.to_string()));
            }
        } else {
            segments.push(Segment::Literal(part.to_string()));
        }
    }
    segments
}

fn compute_specificity(segments: &[Segment], raw_path: &str) -> i64 {
    let mut score: i64 = 0;
    for seg in segments {
        match seg {
            Segment::Literal(_) => score += 10,
            Segment::Param(_) => score += 1,
            Segment::CatchAll(_) => score += 1,
        }
    }
    score += raw_path.len() as i64;
    score
}

/// Internal three-way result for segment matching.
enum SegmentMatch {
    Ok,
    /// A param slot was reached but the value failed validation.
    InvalidParam,
    NoMatch,
}

fn match_segments(
    segments: &[Segment],
    remaining: &str,
    params: &mut Params,
) -> SegmentMatch {
    if segments.is_empty() {
        if remaining.is_empty() {
            return SegmentMatch::Ok;
        }
        return SegmentMatch::NoMatch;
    }

    match &segments[0] {
        Segment::Literal(lit) => {
            let after = if remaining == lit.as_str() {
                ""
            } else if remaining.starts_with(lit.as_str())
                && remaining[lit.len()..].starts_with('/')
            {
                remaining[lit.len() + 1..].trim_start_matches('/')
            } else {
                return SegmentMatch::NoMatch;
            };
            match_segments(&segments[1..], after, params)
        }
        Segment::Param(name) => {
            let (val, rest) = if let Some(idx) = remaining.find('/') {
                (&remaining[..idx], remaining[idx + 1..].trim_start_matches('/'))
            } else {
                (remaining, "")
            };
            if val.is_empty() {
                return SegmentMatch::NoMatch;
            }
            if !is_safe_param(val) {
                // The literal prefix matched — the client supplied an invalid param value.
                return SegmentMatch::InvalidParam;
            }
            params.push((name.clone(), val.to_string()));
            match_segments(&segments[1..], rest, params)
        }
        Segment::CatchAll(name) => {
            if !remaining.is_empty() {
                if !is_safe_catchall(remaining) {
                    // Traversal in a catch-all path → 404 (spec §l2: "../ traversal in URL → 404")
                    return SegmentMatch::NoMatch;
                }
                params.push((name.clone(), remaining.to_string()));
            }
            SegmentMatch::Ok
        }
    }
}

/// Check that a single path param value is safe (no traversal).
fn is_safe_param(val: &str) -> bool {
    if val.contains("..") {
        return false;
    }
    val.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c))
}

/// Check that a catch-all param value is safe.
fn is_safe_catchall(val: &str) -> bool {
    for component in val.split('/') {
        if component == ".." || component.starts_with("..") || component.ends_with("..") {
            return false;
        }
        if !component.chars().all(|c| c.is_alphanumeric() || "-_./".contains(c)) {
            return false;
        }
    }
    true
}

/// Sort routes by specificity descending.
pub fn sort_routes(routes: &mut Vec<Route>) {
    routes.sort_by(|a, b| b.specificity.cmp(&a.specificity));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteConfig;

    #[test]
    fn test_simple_match() {
        let cfg = RouteConfig {
            path: "/assets/{relpath}".to_string(),
            root: "assets/".to_string(),
            tail: None,
        };
        let route = Route::from_config(&cfg);
        let params = match route.match_path("/assets/css/main.css") {
            MatchResult::Matched(p) => p,
            other => panic!("expected Matched, got {:?}", other),
        };
        let relpath = params.iter().find(|(k, _)| k == "relpath").unwrap();
        assert_eq!(relpath.1, "css/main.css");
    }

    #[test]
    fn test_param_match() {
        let cfg = RouteConfig {
            path: "/content/posts/{stem}/{filename}".to_string(),
            root: "content/posts/{stem}/".to_string(),
            tail: None,
        };
        let route = Route::from_config(&cfg);
        let params = match route.match_path("/content/posts/hello/index.html") {
            MatchResult::Matched(p) => p,
            other => panic!("expected Matched, got {:?}", other),
        };
        let stem = params.iter().find(|(k, _)| k == "stem").unwrap();
        let filename = params.iter().find(|(k, _)| k == "filename").unwrap();
        assert_eq!(stem.1, "hello");
        assert_eq!(filename.1, "index.html");
    }

    #[test]
    fn test_traversal_rejected() {
        let cfg = RouteConfig {
            path: "/assets/{relpath}".to_string(),
            root: "assets/".to_string(),
            tail: None,
        };
        let route = Route::from_config(&cfg);
        // `..` in relpath → NoMatch → 404 (spec §l2: "../ traversal in URL → 404")
        assert_eq!(route.match_path("/assets/../secret"), MatchResult::NoMatch);
    }

    #[test]
    fn test_no_match() {
        let cfg = RouteConfig {
            path: "/assets/{relpath}".to_string(),
            root: "assets/".to_string(),
            tail: None,
        };
        let route = Route::from_config(&cfg);
        assert_eq!(route.match_path("/other/path"), MatchResult::NoMatch);
    }

    #[test]
    fn test_invalid_param_returns_invalid_param() {
        let cfg = RouteConfig {
            path: "/posts/{stem}".to_string(),
            root: "posts/".to_string(),
            tail: None,
        };
        let route = Route::from_config(&cfg);
        // `..` inside a Param segment should yield InvalidParam.
        assert_eq!(route.match_path("/posts/.."), MatchResult::InvalidParam);
    }
}
