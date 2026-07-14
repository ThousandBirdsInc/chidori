//! SSRF guard for the guest-facing `http`/`fetch` host effect.
//!
//! Guest agents hand `execute_http` arbitrary URLs, and under OS isolation the
//! request is brokered by the parent process — which has the host's full
//! network reach — so without a guard an agent can pivot into cloud metadata
//! services (169.254.169.254), loopback daemons, and RFC 1918 internal
//! networks. This module blocks requests whose destination resolves to a
//! non-public address, and it does so at the right layer:
//!
//!  * **DNS resolution time.** The guard is installed as the reqwest client's
//!    DNS resolver ([`dns_resolver`]), so the addresses that get filtered are
//!    the exact addresses the connector would dial. That closes the classic
//!    DNS-rebinding TOCTOU (resolve-check-resolve-connect) — there is no
//!    second resolution to rebind.
//!  * **Every redirect hop.** [`redirect_policy`] re-checks each hop's scheme
//!    and IP-literal host, and hostname hops go back through the guarded
//!    resolver when the connector dials them, so a public server can't 302 the
//!    client into a private address.
//!  * **Pre-flight** ([`preflight`]) for the initial URL, so IP-literal
//!    requests fail with a clear policy error instead of a transport error.
//!
//! Escape hatches, because loopback is legitimately used by host-injected
//! endpoints and local development:
//!
//!  * `CHIDORI_HTTP_ALLOW_HOSTS` — comma-separated hostnames, IPs, or CIDRs
//!    that may resolve to otherwise-blocked addresses
//!    (e.g. `localhost,10.0.0.0/8`). The single value `*` disables the guard.
//!  * [`trust_host`] — host code registers endpoints it injected itself (the
//!    mock-integration gateway rewrite target, the app-data plane endpoint,
//!    test fixtures). Guest code never reaches this function.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, RwLock};

/// Env var listing extra allowed destinations (hostnames, IPs, CIDRs, or `*`).
pub const ALLOW_HOSTS_ENV: &str = "CHIDORI_HTTP_ALLOW_HOSTS";

/// Hosts registered at runtime by host code ([`trust_host`]): the mock-gateway
/// rewrite target, the app-data endpoint, and test fixtures. Add-only, keyed
/// by lowercase hostname (no port).
static TRUSTED_HOSTS: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// Rules parsed from `CHIDORI_HTTP_ALLOW_HOSTS`, once per process — the same
/// process-global env assumption `apply_base_url_override` documents (each
/// agent run is its own subprocess with its own env).
static ENV_RULES: LazyLock<AllowRules> = LazyLock::new(|| {
    AllowRules::parse(std::env::var(ALLOW_HOSTS_ENV).unwrap_or_default().as_str())
});

/// Parsed form of the allowlist env var.
#[derive(Debug, Default, PartialEq, Eq)]
struct AllowRules {
    /// `*` was present: the guard is disabled entirely.
    allow_all: bool,
    /// Lowercase hostnames (and IP literals in their canonical string form).
    hosts: HashSet<String>,
    /// CIDR ranges, as (network address, prefix length).
    cidrs: Vec<(IpAddr, u8)>,
}

impl AllowRules {
    fn parse(raw: &str) -> Self {
        let mut rules = AllowRules::default();
        for item in raw.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if item == "*" {
                rules.allow_all = true;
            } else if let Some((net, prefix)) = parse_cidr(item) {
                rules.cidrs.push((net, prefix));
            } else if let Ok(ip) = item.parse::<IpAddr>() {
                // Store the canonical form so `[::1]`-style URL hosts and
                // differently-written literals still match.
                rules.hosts.insert(ip.to_string());
            } else {
                rules.hosts.insert(item.to_ascii_lowercase());
            }
        }
        rules
    }

    fn host_allowed(&self, host: &str) -> bool {
        if self.allow_all {
            return true;
        }
        let host = canonical_host(host);
        self.hosts.contains(&host)
    }

    fn ip_allowed(&self, ip: IpAddr) -> bool {
        if self.allow_all {
            return true;
        }
        self.cidrs
            .iter()
            .any(|(net, prefix)| cidr_contains(*net, *prefix, ip))
    }
}

/// Lowercase and canonicalize a URL host for allowlist lookups: strip IPv6
/// brackets and normalize IP literals to their canonical string form.
fn canonical_host(host: &str) -> String {
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return ip.to_string();
    }
    host.to_ascii_lowercase()
}

fn parse_cidr(item: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = item.split_once('/')?;
    let addr = addr.trim().parse::<IpAddr>().ok()?;
    let prefix = prefix.trim().parse::<u8>().ok()?;
    let max = match addr {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    (prefix <= max).then_some((addr, prefix))
}

fn cidr_contains(net: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            (u32::from(net) & mask) == (u32::from(ip) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            (u128::from(net) & mask) == (u128::from(ip) & mask)
        }
        _ => false,
    }
}

/// Register a host-injected endpoint (by URL host, no port) as trusted, so the
/// guard lets requests through to it even when it lives on loopback. Only host
/// code calls this — with hosts it configured itself — never with anything a
/// guest supplied.
pub fn trust_host(host: &str) {
    TRUSTED_HOSTS
        .write()
        .expect("SSRF trusted-host lock poisoned")
        .insert(canonical_host(host));
}

fn host_is_trusted(host: &str) -> bool {
    TRUSTED_HOSTS
        .read()
        .expect("SSRF trusted-host lock poisoned")
        .contains(&canonical_host(host))
}

/// True when `host` may reach otherwise-blocked addresses: allowlisted via the
/// env var or registered through [`trust_host`].
fn host_exempt(host: &str) -> bool {
    ENV_RULES.host_allowed(host) || host_is_trusted(host)
}

/// Should the guard refuse to connect to `ip`? Everything that is not
/// unambiguously a public unicast address is blocked: loopback, RFC 1918
/// private, link-local (the cloud-metadata range), CGNAT, benchmarking,
/// documentation, multicast, reserved, and their IPv6 counterparts
/// (including IPv4-mapped/compatible forms, which unwrap to the IPv4 rules).
fn ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_blocked(v4),
        IpAddr::V6(v6) => ipv6_blocked(v6),
    }
}

fn ipv4_blocked(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254.0.0/16 — cloud metadata lives here
        || ip.is_broadcast()
        || ip.is_documentation()
        || octets[0] == 0 // 0.0.0.0/8 "this network"
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || octets[0] >= 224 // 224.0.0.0/4 multicast + 240.0.0.0/4 reserved
}

fn ipv6_blocked(ip: Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) and the deprecated IPv4-compatible form
    // both smuggle an IPv4 destination; judge them by the IPv4 rules.
    if let Some(v4) = ip.to_ipv4() {
        return ipv4_blocked(v4);
    }
    let segments = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (segments[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
        || (segments[0] == 0x2001 && segments[1] == 0xdb8) // 2001:db8::/32 documentation
        || (segments[0] == 0x64 && segments[1] == 0xff9b) // 64:ff9b::/96 NAT64 → IPv4
}

fn blocked_message(host: &str, ip: IpAddr) -> String {
    format!(
        "SSRF protection: '{host}' resolves to non-public address {ip}, which the http effect \
         refuses to reach; set {ALLOW_HOSTS_ENV} (comma-separated hostnames, IPs, or CIDRs; \
         '*' disables the guard) to allow it"
    )
}

/// Check a URL before the request (and on every redirect hop): scheme must be
/// http(s), and an IP-literal host must not be a blocked address. Hostname
/// destinations are checked later, by the guarded resolver, against the exact
/// addresses the connector will dial.
pub fn preflight(url: &url::Url) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "SSRF protection: scheme '{other}' is not allowed for the http effect (use http or https)"
            ));
        }
    }
    let Some(host) = url.host_str() else {
        return Err("SSRF protection: url has no host".to_string());
    };
    if host_exempt(host) {
        return Ok(());
    }
    if let Ok(ip) = canonical_host(host).parse::<IpAddr>() {
        if ip_blocked(ip) && !ENV_RULES.ip_allowed(ip) {
            return Err(blocked_message(host, ip));
        }
    }
    Ok(())
}

/// Filter the addresses `host` resolved to, dropping blocked ones. Errors when
/// every address is blocked (so the caller gets the policy message instead of
/// an empty-lookup error).
fn filter_resolved(host: &str, addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    if host_exempt(host) {
        return Ok(addrs);
    }
    let mut blocked: Option<IpAddr> = None;
    let allowed: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|addr| {
            let ip = addr.ip();
            if !ip_blocked(ip) || ENV_RULES.ip_allowed(ip) {
                true
            } else {
                blocked = Some(ip);
                false
            }
        })
        .collect();
    match (allowed.is_empty(), blocked) {
        (true, Some(ip)) => Err(blocked_message(host, ip)),
        _ => Ok(allowed),
    }
}

/// DNS resolver for the http-effect reqwest client: resolves via the system
/// resolver, then applies [`filter_resolved`] so blocked addresses never reach
/// the connector — including on redirect hops and DNS-rebinding flips.
pub struct GuardedDns;

impl reqwest::dns::Resolve for GuardedDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port 0 placeholder: the connector swaps in the URL's port.
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            match filter_resolved(&host, addrs) {
                Ok(allowed) => {
                    Ok(Box::new(allowed.into_iter())
                        as Box<dyn Iterator<Item = SocketAddr> + Send>)
                }
                Err(message) => Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

/// The guarded resolver, shared by the process-wide http-effect client.
pub fn dns_resolver() -> Arc<GuardedDns> {
    Arc::new(GuardedDns)
}

/// Redirect policy for the http-effect client: keep reqwest's 10-hop cap, and
/// re-run [`preflight`] on every hop so redirects can't step outside the
/// policy (IP-literal hops are rejected here; hostname hops are checked by the
/// guarded resolver when dialed).
pub fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > 10 {
            return attempt.error("too many redirects");
        }
        match preflight(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(message),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_metadata_loopback_private_and_v6_equivalents() {
        for addr in [
            "169.254.169.254", // cloud metadata
            "127.0.0.1",
            "127.8.9.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1", // CGNAT
            "198.18.0.1", // benchmarking
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.251", // multicast
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:169.254.169.254", // v4-mapped metadata
            "64:ff9b::a9fe:a9fe",     // NAT64-embedded metadata
        ] {
            assert!(ip_blocked(ip(addr)), "{addr} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for addr in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!ip_blocked(ip(addr)), "{addr} should be allowed");
        }
    }

    #[test]
    fn allow_rules_parse_hosts_ips_cidrs_and_star() {
        let rules = AllowRules::parse(" localhost , 10.0.0.0/8, ::1 , internal.corp ");
        assert!(!rules.allow_all);
        assert!(rules.host_allowed("LOCALHOST"));
        assert!(rules.host_allowed("internal.corp"));
        assert!(rules.host_allowed("[::1]"));
        assert!(rules.ip_allowed(ip("10.20.30.40")));
        assert!(!rules.ip_allowed(ip("192.168.0.1")));

        let star = AllowRules::parse("*");
        assert!(star.allow_all);
        assert!(star.host_allowed("anything"));
        assert!(star.ip_allowed(ip("169.254.169.254")));
    }

    #[test]
    fn preflight_rejects_blocked_ip_literals_and_bad_schemes() {
        let err = preflight(&url::Url::parse("http://169.254.169.254/latest/meta-data").unwrap())
            .unwrap_err();
        assert!(err.contains("SSRF protection"), "{err}");
        assert!(err.contains(ALLOW_HOSTS_ENV), "{err}");

        let err = preflight(&url::Url::parse("ftp://example.com/x").unwrap()).unwrap_err();
        assert!(err.contains("scheme"), "{err}");

        preflight(&url::Url::parse("https://example.com/").unwrap()).unwrap();
    }

    #[test]
    fn preflight_honors_trust_host_registration() {
        let target = url::Url::parse("http://[::1]:9999/internal").unwrap();
        assert!(preflight(&target).is_err());
        trust_host("[::1]");
        preflight(&target).unwrap();
    }

    #[test]
    fn filter_resolved_drops_blocked_addrs_and_errors_when_none_survive() {
        let public: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let private: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let kept = filter_resolved("example.com", vec![public, private]).unwrap();
        assert_eq!(kept, vec![public]);

        let err = filter_resolved("rebind.example", vec![private]).unwrap_err();
        assert!(err.contains("SSRF protection"), "{err}");

        // A trusted host keeps its blocked addresses.
        trust_host("app-data.internal");
        let kept = filter_resolved("app-data.internal", vec![private]).unwrap();
        assert_eq!(kept, vec![private]);
    }
}
