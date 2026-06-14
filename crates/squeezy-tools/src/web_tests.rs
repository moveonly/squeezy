use super::*;

#[test]
fn ssrf_predicate_blocks_internal_targets_and_allows_public() {
    let blocked = [
        "127.0.0.1",
        "::1",
        "169.254.169.254",
        "10.0.0.1",
        "192.168.1.1",
        "172.16.0.1",
        "fc00::1",
        "fe80::1",
        "0.0.0.0",
    ];
    for raw in blocked {
        let ip: IpAddr = raw.parse().expect("parse ip");
        assert!(ip_is_blocked(&ip), "expected {raw} to be blocked");
    }

    let allowed = [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "2606:4700:4700::1111",
    ];
    for raw in allowed {
        let ip: IpAddr = raw.parse().expect("parse ip");
        assert!(!ip_is_blocked(&ip), "expected {raw} to be allowed");
    }
}

#[tokio::test]
async fn ensure_url_allowed_rejects_loopback_and_metadata_and_localhost() {
    for raw in [
        "http://127.0.0.1/",
        "http://[::1]/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5:8080/admin",
        "http://192.168.0.1/",
        "http://localhost:6379/",
    ] {
        let url = Url::parse(raw).expect("parse url");
        assert!(
            ensure_url_allowed(&url).await.is_err(),
            "expected {raw} to be refused"
        );
    }
}

#[tokio::test]
async fn ensure_url_allowed_permits_public_literal_ip() {
    let url = Url::parse("http://1.1.1.1/").expect("parse url");
    assert!(ensure_url_allowed(&url).await.is_ok());
}

#[cfg(test)]
mod ssrf_range_tests {
    use super::*;

    #[test]
    fn ip_is_blocked_covers_cgnat_this_network_and_broadcast() {
        // RFC 6598 shared address space / CGNAT and the rest of 0.0.0.0/8 were
        // previously not blocked, allowing SSRF to internal/shared-tenancy
        // services. They must now be refused.
        let blocked = [
            "100.64.0.1",
            "100.96.0.1",
            "100.127.255.255",
            "0.1.2.3",
            "192.0.0.1",
            "255.255.255.255",
        ];
        for raw in blocked {
            let ip: IpAddr = raw.parse().expect("parse ip");
            assert!(ip_is_blocked(&ip), "expected {raw} to be blocked");
        }

        // Adjacent public addresses just outside the CGNAT range stay allowed.
        let allowed = ["100.63.255.255", "100.128.0.1"];
        for raw in allowed {
            let ip: IpAddr = raw.parse().expect("parse ip");
            assert!(!ip_is_blocked(&ip), "expected {raw} to be allowed");
        }
    }

    #[tokio::test]
    async fn ensure_url_allowed_returns_validated_literal_ip() {
        // The validated IP is returned so the caller can pin the dialed
        // address, closing the DNS-rebinding TOCTOU.
        let url = Url::parse("http://1.1.1.1/").expect("parse url");
        assert_eq!(
            ensure_url_allowed(&url).await,
            Ok(IpAddr::from([1, 1, 1, 1]))
        );

        let url = Url::parse("http://100.64.0.1/").expect("parse url");
        assert!(ensure_url_allowed(&url).await.is_err());
    }
}
