//! Network egress control — given a provider `baseUrl`, decide whether it's
//! a private/local endpoint (allowed when the route is `Private`) or a cloud
//! endpoint (blocked when the route is `Private`).
//!
//! Direct port of the Electron app's `isPrivateBaseUrl` in
//! `src/store/catalog.js`. The IPv4 check is the load-bearing part: we
//! require the hostname to be an actual dotted-quad before applying the
//! private-range check, so a public name like `10.evil.com` doesn't slip
//! through and skip privacy screening.
//!
//! Local / loopback / private ranges covered:
//!   - `localhost`, `::1`, `[::1]`
//!   - `127.0.0.0/8`        loopback
//!   - `10.0.0.0/8`         RFC 1918
//!   - `192.168.0.0/16`     RFC 1918
//!   - `172.16.0.0/12`      RFC 1918
//!   - `100.64.0.0/10`      Tailscale CGNAT
//!
//! Named private-network suffixes (`.local`, `.lan`, `.internal`, `.ts.net`) are NOT
//! auto-trusted — they require explicit enrollment by the caller. Use
//! [`is_private_endpoint_trusted_by_name`] to identify such names without granting
//! network access.

/// Returns `true` if `base_url` points to a private/local endpoint.
///
/// Anything that fails to parse as a URL is treated as public (refuse) — we
/// don't want a typo in the base URL to silently let traffic through.
pub fn is_private_endpoint(base_url: &str) -> bool {
    let url = match url::Url::parse(base_url) {
        Ok(u) => u,
        Err(_) => return false,
    };

    let host = match url.host_str() {
        Some(h) => h,
        None => return false,
    };

    // Hostname literal "localhost" or IPv6 loopback. `url::Url::host_str`
    // returns IPv6 literals *with* brackets, e.g. "[::1]".
    if host.eq_ignore_ascii_case("localhost") || host == "::1" || host == "[::1]" {
        return true;
    }

    // IPv4: only treat the hostname as private if it's a real dotted-quad
    // with each octet 0..=255. Otherwise "10.evil.com" would match `a === 10`
    // and slip through.
    let octets: Vec<&str> = host.split('.').collect();
    let is_ipv4 = octets.len() == 4
        && octets
            .iter()
            .all(|o| o.parse::<u16>().map(|n| n <= 255).unwrap_or(false));
    if is_ipv4 {
        let a: u16 = octets[0].parse().unwrap();
        let b: u16 = octets[1].parse().unwrap();
        if a == 127 {
            return true;
        } // loopback
        if a == 10 {
            return true;
        } // RFC 1918
        if a == 192 && b == 168 {
            return true;
        } // RFC 1918
        if a == 172 && (16..=31).contains(&b) {
            return true;
        } // RFC 1918
        if a == 100 && (64..=127).contains(&b) {
            return true;
        } // Tailscale CGNAT
        return false;
    }

    // Hostname is not a private IP literal — do not trust by name alone.
    false
}

/// Returns whether `base_url` is considered private solely because its host
/// carries a trusted private-network name suffix. These names are not resolved
/// or authenticated here, so the UI must tell the user that this trust is safe
/// only on a network they control.
pub fn is_private_endpoint_trusted_by_name(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let lower = host.to_ascii_lowercase();
    lower.ends_with(".local")
        || lower.ends_with(".lan")
        || lower.ends_with(".internal")
        || lower.ends_with(".ts.net")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_by_name_distinguishes_names_from_private_literals() {
        // Suffix-based names are NOT auto-trusted by is_private_endpoint (H-05).
        for url in [
            "http://model.local:1234/v1",
            "http://host.lan/v1",
            "https://node.internal/v1",
            "https://machine.tailnet.ts.net/v1",
        ] {
            assert!(
                !is_private_endpoint(url),
                "suffix-based names must not be auto-trusted: {url}"
            );
            assert!(is_private_endpoint_trusted_by_name(url));
        }
        // Loopback, private IPs, and public names are unaffected.
        for url in [
            "http://localhost:1234/v1",
            "http://127.0.0.1:1234/v1",
            "http://10.0.0.5/v1",
            "https://api.example.com/v1",
            "not a url",
        ] {
            assert!(!is_private_endpoint_trusted_by_name(url));
        }
    }

    #[test]
    fn loopback_is_always_private() {
        // IPv4 loopback
        assert!(is_private_endpoint("http://127.0.0.1:1234/v1"));
        assert!(is_private_endpoint("http://127.255.255.255:80"));
        // IPv6 / localhost literals
        assert!(is_private_endpoint("http://localhost:1234/v1"));
        assert!(is_private_endpoint("http://[::1]:8080/v1"));
    }

    #[test]
    fn suffix_names_cannot_slip_as_private() {
        // A .ts.net name must NOT be treated as private (H-05).
        assert!(!is_private_endpoint("https://friday.tail.ts.net:8765"));
        // A .local name must NOT be treated as private.
        assert!(!is_private_endpoint("http://nas.local:5000"));
        // A .lan name must NOT be treated as private.
        assert!(!is_private_endpoint("http://tadashi.lan:8080/v1"));
        // A .internal name must NOT be treated as private.
        assert!(!is_private_endpoint("http://server.internal:80"));
        // A hostname ending in a substring of a private suffix must not be caught.
        assert!(!is_private_endpoint("https://evil.local.com/v1"));
    }

    #[test]
    fn public_ips_are_not_private() {
        // Public DNS (8.8.8.8, 1.1.1.1) is not private.
        assert!(!is_private_endpoint("http://8.8.8.8:80"));
        assert!(!is_private_endpoint("http://1.1.1.1:53"));
    }

    #[test]
    fn malformed_input_is_not_private() {
        assert!(!is_private_endpoint("not a url"));
        assert!(!is_private_endpoint(""));
    }

    #[test]
    fn private_ipv4_ranges_are_private() {
        assert!(is_private_endpoint("http://10.0.0.5:8080/v1"));
        assert!(is_private_endpoint("http://10.255.255.255:80"));
        assert!(is_private_endpoint("http://192.168.1.1:80"));
        assert!(is_private_endpoint("http://172.16.0.1:80"));
        assert!(is_private_endpoint("http://172.31.255.255:80"));
        assert!(is_private_endpoint("http://100.97.80.2:8765"));
        assert!(is_private_endpoint("http://100.64.0.1:80"));
        assert!(is_private_endpoint("http://100.127.255.254:80"));

        // Just outside private ranges
        assert!(!is_private_endpoint("http://172.15.0.1:80"));
        assert!(!is_private_endpoint("http://172.32.0.1:80"));
        assert!(!is_private_endpoint("http://100.63.255.255:80"));
        assert!(!is_private_endpoint("http://100.128.0.0:80"));
    }
}
