use anyhow::Result;
use regex::Regex;

use crate::config::RouteConfig;

enum Matcher {
    Exact(String),
    Regex(Regex),
}

struct Route {
    matcher: Matcher,
    target: String,
    priority: i32,
}

impl Route {
    fn matches(&self, host: &str) -> bool {
        match &self.matcher {
            Matcher::Exact(h) => h.eq_ignore_ascii_case(host),
            Matcher::Regex(re) => re.is_match(host),
        }
    }
}

/// Resolves a `Host` header to a target URL, mirroring Traefik's exact
/// `Host()` vs. regex `HostRegexp()` matching with priority as the
/// tie-breaker (higher priority wins).
pub struct Router {
    routes: Vec<Route>,
}

impl Router {
    pub fn new(configs: &[RouteConfig]) -> Result<Self> {
        let mut routes = Vec::with_capacity(configs.len());
        for c in configs {
            let matcher = if let Some(host) = &c.host {
                Matcher::Exact(host.clone())
            } else if let Some(pattern) = &c.host_regex {
                Matcher::Regex(Regex::new(pattern)?)
            } else {
                // Config::validate() guarantees one of the two is set.
                unreachable!("route with neither host nor host_regex");
            };
            routes.push(Route {
                matcher,
                target: c.target.clone(),
                priority: c.priority,
            });
        }
        routes.sort_by_key(|r| std::cmp::Reverse(r.priority));
        Ok(Self { routes })
    }

    pub fn resolve(&self, host: &str) -> Option<&str> {
        let host = host.split(':').next().unwrap_or(host);
        self.routes
            .iter()
            .find(|r| r.matches(host))
            .map(|r| r.target.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: Option<&str>, host_regex: Option<&str>, target: &str, priority: i32) -> RouteConfig {
        RouteConfig {
            host: host.map(String::from),
            host_regex: host_regex.map(String::from),
            target: target.to_string(),
            priority,
        }
    }

    #[test]
    fn exact_match_wins_over_regex_by_priority() {
        let router = Router::new(&[
            route(None, Some(r".+\.myapp\.test"), "http://localhost:3010", 10),
            route(Some("api.myapp.test"), None, "http://localhost:3011", 20),
        ])
        .unwrap();

        assert_eq!(
            router.resolve("api.myapp.test"),
            Some("http://localhost:3011")
        );
        assert_eq!(
            router.resolve("whatever.myapp.test"),
            Some("http://localhost:3010")
        );
    }

    #[test]
    fn strips_port_from_host_header() {
        let router = Router::new(&[route(Some("api.myapp.test"), None, "http://localhost:3011", 20)]).unwrap();
        assert_eq!(
            router.resolve("api.myapp.test:443"),
            Some("http://localhost:3011")
        );
    }

    #[test]
    fn no_match_returns_none() {
        let router = Router::new(&[route(Some("api.myapp.test"), None, "http://localhost:3011", 20)]).unwrap();
        assert_eq!(router.resolve("unknown.test"), None);
    }

    #[test]
    fn host_is_case_insensitive() {
        let router = Router::new(&[route(Some("api.myapp.test"), None, "http://localhost:3011", 20)]).unwrap();
        assert_eq!(
            router.resolve("API.MyApp.Test"),
            Some("http://localhost:3011")
        );
    }
}
