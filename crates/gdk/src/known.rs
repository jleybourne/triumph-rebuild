//! Every runtime GUID we can name: the ones this crate serves, and the ones the fork's design names
//! for areas not served here yet (display, capture, game saves, game UI, local storage, speech, the
//! user's devices). For `gdkguids` and the G1 table.

use crate::guid::Guid;

/// (GUID, area, role, served here).
pub static KNOWN: &[(Guid, &str, &str, bool)] = &[
    (Guid::parse("073b7dcb-1fcf-4030-94be-e3c9eb623428"), "task queue / async", "provider = iid", true),
    (Guid::parse("01acd177-91f9-4763-a38e-ccbb55ce32e0"), "user", "provider (also the iid the title asks with)", true),
    (Guid::parse("cef4fac0-7676-4a94-a119-4c43f9eb5b74"), "user", "gamertag iid", true),
    (Guid::parse("eb9bf948-18dc-4d82-bbcc-40e0a809c4c0"), "user", "iid (older titles)", false),
    (Guid::parse("7d824997-10dc-45ab-86b7-2737767c0bf1"), "user devices", "provider", false),
    (Guid::parse("0cc6a956-e7e1-4fdf-9341-9d5da649ebc8"), "user devices", "iid", false),
    (Guid::parse("973a344e-24bf-4d0f-8457-56c534892b29"), "title", "provider", true),
    (Guid::parse("2549f142-6419-4a06-97b5-931aab7c2f34"), "title", "iid", true),
    (Guid::parse("8836fe87-edb9-4fe3-8dad-05f0d2cd5b40"), "features", "provider = iid", true),
    (Guid::parse("0651aae2-4012-4077-bf84-8b9097090e2c"), "invites", "provider = iid", true),
    (Guid::parse("bbfbdcc7-bfe7-409b-a5ca-edf054960b4d"), "events", "provider = iid", true),
    (Guid::parse("e349bd1a-fc20-4e40-b99c-4178cc6b409f"), "system", "provider", true),
    (Guid::parse("1861cf2e-e18b-4834-a9f5-b4a4e6efb4cf"), "system", "iid", true),
    (Guid::parse("6fd71f09-7513-49f0-89bc-bfaf5df6f852"), "system", "iid (older titles)", false),
    (Guid::parse("b884675d-b738-4a9c-815d-9a9a1e0c6c9b"), "analytics", "provider = iid", true),
    (Guid::parse("af406016-e850-4aa8-a88d-2f3dcb9dac7e"), "package", "provider", true),
    (Guid::parse("0b9d556e-ab70-43f4-baa0-9999ac4cb040"), "package", "iid", true),
    (Guid::parse("0dd112ac-7c24-448c-b92b-3960fb5bd30c"), "store", "provider", true),
    (Guid::parse("b09d803c-2414-4a05-82c6-66dfdc9e9a44"), "store", "iid", true),
    (Guid::parse("2d42fea5-e71d-4b76-97cd-c50afbb3ae5d"), "store", "iid with durables", true),
    (Guid::parse("37e56907-2f10-41e8-b72f-36edb185331a"), "networking", "provider (also an iid)", true),
    (Guid::parse("bf2346b2-39af-4658-b5ea-44713c7e83b3"), "networking", "iid", true),
    (Guid::parse("03f0fe74-fdd9-4e5c-b630-f9339c47acc5"), "display (HDR)", "provider", false),
    (Guid::parse("35f07670-706e-4bfb-9476-090798c5ebf3"), "display (HDR)", "iid", false),
    (Guid::parse("1b339674-328d-4283-a200-3171f18d3639"), "shell (URIs, deferrals)", "provider = iid", false),
    (Guid::parse("a4f1aee2-4bf1-4485-b008-a7c26d52ac27"), "capture", "provider", false),
    (Guid::parse("22e672d7-b4e3-406c-bd50-8f0d25236f9e"), "capture", "iid", false),
    (Guid::parse("704c3f58-e629-4cc2-b197-30511b996fe2"), "game save", "provider (also an iid)", false),
    (Guid::parse("1bfff3af-f14a-40a3-8e35-9ada906593f9"), "game save", "iid", false),
    (Guid::parse("dfcd4649-4ff8-4043-ba07-35d607df98b0"), "game UI", "provider", false),
    (Guid::parse("eaf669df-5542-4590-99a3-8dc061f837cc"), "game UI", "iid", false),
    (Guid::parse("f4faf4d4-2d04-4fce-b3e0-474a713a3e84"), "local storage", "provider", false),
    (Guid::parse("41a4e10c-5a7e-41d9-8c37-37bde62a07d6"), "local storage", "iid", false),
    (Guid::parse("d29411df-0794-4553-8b27-95fc02d0f75d"), "local storage", "iid (older titles)", false),
    (Guid::parse("3e241977-6237-41e9-8559-102c2d9983f1"), "accessibility / speech", "provider", false),
    (Guid::parse("d722b373-8c4d-4692-9e51-d6ad9b37aa7d"), "accessibility / speech", "iid", false),
];

pub fn name(g: &Guid) -> Option<(&'static str, &'static str, bool)> {
    KNOWN.iter().find(|k| k.0 == *g).map(|k| (k.1, k.2, k.3))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Served" here means `QueryApiImpl` answers some pair with it.
    #[test]
    fn served_flags_match_the_table() {
        for (g, area, _, served) in KNOWN {
            let in_table = crate::table::ALL.iter().any(|d| d.provider == *g || d.iids.contains(g));
            assert_eq!(in_table, *served, "{area} {g}");
        }
        let mut all: Vec<Guid> = KNOWN.iter().map(|k| k.0).collect();
        all.sort_by_key(|g| g.to_bytes());
        all.dedup();
        assert_eq!(all.len(), KNOWN.len());
    }
}
