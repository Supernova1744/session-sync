use rand::Rng;
use uuid::Uuid;

fn ts_hex(ms: i64) -> String {
    format!("{:08x}", (ms as u64) & 0xFFFF_FFFF)
}

fn uuid_tail() -> String {
    let full = Uuid::new_v4().simple().to_string();
    full[8..].to_string() // 24 chars
}

pub fn new_session_id(created_ms: i64) -> String {
    format!("ses_{}{}", ts_hex(created_ms), uuid_tail())
}

pub fn new_message_id(created_ms: i64) -> String {
    format!("msg_{}{}", ts_hex(created_ms), uuid_tail())
}

pub fn new_part_id(created_ms: i64) -> String {
    format!("prt_{}{}", ts_hex(created_ms), uuid_tail())
}

const ADJECTIVES: &[&str] = &[
    "misty", "swift", "lucky", "bright", "calm", "amber", "neon", "fuzzy", "nimble", "witty",
    "peppy", "vivid", "bold", "crisp", "grand", "noble", "wild", "sleek", "vast", "keen", "warm",
    "cool", "fresh", "gentle",
];

const NOUNS: &[&str] = &[
    "moon", "river", "canyon", "storm", "peak", "engine", "eagle", "orchid", "squid", "panda",
    "thunder", "comet", "forest", "harbor", "bridge", "flame", "dawn", "ridge", "tide", "grove",
    "haze", "spark", "cloud", "drift",
];

pub fn new_slug() -> String {
    let mut rng = rand::thread_rng();
    let adj = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())];
    let noun = NOUNS[rng.gen_range(0..NOUNS.len())];
    format!("{}-{}", adj, noun)
}

pub fn ms_to_iso(ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique() {
        let ids: HashSet<_> = (0..200)
            .map(|_| new_message_id(1_700_000_000_000))
            .collect();
        assert_eq!(ids.len(), 200);
    }

    #[test]
    fn ids_with_increasing_timestamps_sort_correctly() {
        let id1 = new_part_id(1_000_000);
        let id2 = new_part_id(2_000_000);
        assert!(id1 < id2);
    }

    #[test]
    fn slug_has_two_words() {
        let slug = new_slug();
        assert_eq!(slug.chars().filter(|&c| c == '-').count(), 1);
    }
}
