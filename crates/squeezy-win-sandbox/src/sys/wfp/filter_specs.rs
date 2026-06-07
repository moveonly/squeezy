//! Static WFP filter specifications for offline-user egress blocking.
//!
//! Twelve persistent block filters (6 logical × IPv4/IPv6) covering ICMP,
//! DNS (port 53), DNS-over-TLS (port 853), SMB (port 445), and
//! NetBIOS/SMB (port 139).  Every filter also carries a `User` condition that
//! scopes it to the offline sandbox account SID.
//!
//! The GUID constants for provider, sublayer, and each filter key are
//! Squeezy-owned identities.  Do NOT regenerate them — changing them would
//! orphan old persistent WFP objects.

use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4, FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
};
use windows_sys::Win32::Networking::WinSock::{IPPROTO_ICMP, IPPROTO_ICMPV6};
use windows_sys::core::GUID;

/// A condition element within a filter spec.
#[derive(Clone, Copy)]
pub(super) enum ConditionSpec {
    /// Match the offline account SID via `FWPM_CONDITION_ALE_USER_ID`.
    User,
    /// Match a specific IP protocol number via `FWPM_CONDITION_IP_PROTOCOL`.
    Protocol(u8),
    /// Match a specific remote port via `FWPM_CONDITION_IP_REMOTE_PORT`.
    RemotePort(u16),
}

/// One WFP blocking filter: the stable key GUID, display name, layer GUID,
/// and the conditions that must all match for the block to fire.
#[derive(Clone, Copy)]
pub(super) struct FilterSpec {
    pub(super) key: GUID,
    pub(super) name: &'static str,
    pub(super) layer_key: GUID,
    pub(super) conditions: &'static [ConditionSpec],
}

// ── Squeezy-owned WFP identity GUIDs ──────────────────────────────────────────
//
// These are stable, project-specific identifiers.  Invented fresh; they do not
// match any Codex or Microsoft values.

/// Provider GUID — `a3c70d82-f152-4b67-8e31-6d9a7b2c05f4`
pub(super) const PROVIDER_KEY: GUID = GUID {
    data1: 0xa3c7_0d82,
    data2: 0xf152,
    data3: 0x4b67,
    data4: [0x8e, 0x31, 0x6d, 0x9a, 0x7b, 0x2c, 0x05, 0xf4],
};

/// Sublayer GUID — `b8e41f63-2a07-4d9c-a05e-3c71f84d62b1`
pub(super) const SUBLAYER_KEY: GUID = GUID {
    data1: 0xb8e4_1f63,
    data2: 0x2a07,
    data3: 0x4d9c,
    data4: [0xa0, 0x5e, 0x3c, 0x71, 0xf8, 0x4d, 0x62, 0xb1],
};

// ── Filter key GUIDs (one per filter, stable) ─────────────────────────────────

/// ICMP protocol block — ALE_RESOURCE_ASSIGNMENT, IPv4
const KEY_ICMP_ASSIGN_V4: GUID = GUID {
    data1: 0xc1d5_3e70,
    data2: 0x8f4a,
    data3: 0x4e21,
    data4: [0xb6, 0x3f, 0x12, 0xa0, 0x7d, 0x4c, 0x9e, 0x51],
};

/// ICMP protocol block — ALE_RESOURCE_ASSIGNMENT, IPv6
const KEY_ICMPV6_ASSIGN_V6: GUID = GUID {
    data1: 0xd2e6_4f81,
    data2: 0x9b5b,
    data3: 0x4f32,
    data4: [0xc7, 0x40, 0x23, 0xb1, 0x8e, 0x5d, 0xaf, 0x62],
};

/// DNS port 53 block — ALE_AUTH_CONNECT, IPv4
const KEY_DNS_53_V4: GUID = GUID {
    data1: 0xe3f7_5092,
    data2: 0xac6c,
    data3: 0x4043,
    data4: [0xd8, 0x51, 0x34, 0xc2, 0x9f, 0x6e, 0xb0, 0x73],
};

/// DNS port 53 block — ALE_AUTH_CONNECT, IPv6
const KEY_DNS_53_V6: GUID = GUID {
    data1: 0xf408_31a3,
    data2: 0xbd7d,
    data3: 0x4154,
    data4: [0xe9, 0x62, 0x45, 0xd3, 0xa0, 0x7f, 0xc1, 0x84],
};

/// DNS-over-TLS port 853 block — ALE_AUTH_CONNECT, IPv4
const KEY_DOT_853_V4: GUID = GUID {
    data1: 0x0519_42b4,
    data2: 0xce8e,
    data3: 0x4265,
    data4: [0xfa, 0x73, 0x56, 0xe4, 0xb1, 0x80, 0xd2, 0x95],
};

/// DNS-over-TLS port 853 block — ALE_AUTH_CONNECT, IPv6
const KEY_DOT_853_V6: GUID = GUID {
    data1: 0x1628_53c5,
    data2: 0xdf9f,
    data3: 0x4376,
    data4: [0x0b, 0x84, 0x67, 0xf5, 0xc2, 0x91, 0xe3, 0xa6],
};

/// SMB port 445 block — ALE_AUTH_CONNECT, IPv4
const KEY_SMB_445_V4: GUID = GUID {
    data1: 0x2739_64d6,
    data2: 0xf0a0,
    data3: 0x4487,
    data4: [0x1c, 0x95, 0x78, 0x06, 0xd3, 0xa2, 0xf4, 0xb7],
};

/// SMB port 445 block — ALE_AUTH_CONNECT, IPv6
const KEY_SMB_445_V6: GUID = GUID {
    data1: 0x384a_75e7,
    data2: 0x01b1,
    data3: 0x4598,
    data4: [0x2d, 0xa6, 0x89, 0x17, 0xe4, 0xb3, 0x05, 0xc8],
};

/// NetBIOS/SMB port 139 block — ALE_AUTH_CONNECT, IPv4
const KEY_SMB_139_V4: GUID = GUID {
    data1: 0x498b_86f8,
    data2: 0x12c2,
    data3: 0x4609,
    data4: [0x3e, 0xb7, 0x9a, 0x28, 0xf5, 0xc4, 0x16, 0xd9],
};

/// NetBIOS/SMB port 139 block — ALE_AUTH_CONNECT, IPv6
const KEY_SMB_139_V6: GUID = GUID {
    data1: 0x5ac9_97e9,
    data2: 0x23d3,
    data3: 0x471a,
    data4: [0x4f, 0xc8, 0xab, 0x39, 0x06, 0xd5, 0x27, 0xea],
};

/// ICMP (connect layer) block — ALE_AUTH_CONNECT, IPv4
const KEY_ICMP_CONNECT_V4: GUID = GUID {
    data1: 0x6bda_a8fa,
    data2: 0x34e4,
    data3: 0x482b,
    data4: [0x5f, 0xd9, 0xbc, 0x4a, 0x17, 0xe6, 0x38, 0xfb],
};

/// ICMP (connect layer) block — ALE_AUTH_CONNECT, IPv6
const KEY_ICMPV6_CONNECT_V6: GUID = GUID {
    data1: 0x7ceb_b90b,
    data2: 0x45f5,
    data3: 0x493c,
    data4: [0x6f, 0xea, 0xcd, 0x5b, 0x28, 0xf7, 0x49, 0x0c],
};

// ── The 12 filter specs ───────────────────────────────────────────────────────

pub(super) const FILTER_SPECS: &[FilterSpec] = &[
    // ICMP — resource-assignment layer (prevents raw socket binding), IPv4
    FilterSpec {
        key: KEY_ICMP_ASSIGN_V4,
        name: "squeezy_wfp_icmp_assign_v4",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    // ICMP — resource-assignment layer, IPv6
    FilterSpec {
        key: KEY_ICMPV6_ASSIGN_V6,
        name: "squeezy_wfp_icmpv6_assign_v6",
        layer_key: FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    // ICMP — auth-connect layer (belt-and-suspenders), IPv4
    FilterSpec {
        key: KEY_ICMP_CONNECT_V4,
        name: "squeezy_wfp_icmp_connect_v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMP as u8),
        ],
    },
    // ICMP — auth-connect layer, IPv6
    FilterSpec {
        key: KEY_ICMPV6_CONNECT_V6,
        name: "squeezy_wfp_icmpv6_connect_v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[
            ConditionSpec::User,
            ConditionSpec::Protocol(IPPROTO_ICMPV6 as u8),
        ],
    },
    // DNS port 53, IPv4
    FilterSpec {
        key: KEY_DNS_53_V4,
        name: "squeezy_wfp_dns_53_v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    // DNS port 53, IPv6
    FilterSpec {
        key: KEY_DNS_53_V6,
        name: "squeezy_wfp_dns_53_v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(53)],
    },
    // DNS-over-TLS port 853, IPv4
    FilterSpec {
        key: KEY_DOT_853_V4,
        name: "squeezy_wfp_dot_853_v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    // DNS-over-TLS port 853, IPv6
    FilterSpec {
        key: KEY_DOT_853_V6,
        name: "squeezy_wfp_dot_853_v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(853)],
    },
    // SMB port 445, IPv4
    FilterSpec {
        key: KEY_SMB_445_V4,
        name: "squeezy_wfp_smb_445_v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    // SMB port 445, IPv6
    FilterSpec {
        key: KEY_SMB_445_V6,
        name: "squeezy_wfp_smb_445_v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(445)],
    },
    // NetBIOS/SMB port 139, IPv4
    FilterSpec {
        key: KEY_SMB_139_V4,
        name: "squeezy_wfp_smb_139_v4",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
    // NetBIOS/SMB port 139, IPv6
    FilterSpec {
        key: KEY_SMB_139_V6,
        name: "squeezy_wfp_smb_139_v6",
        layer_key: FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        conditions: &[ConditionSpec::User, ConditionSpec::RemotePort(139)],
    },
];
