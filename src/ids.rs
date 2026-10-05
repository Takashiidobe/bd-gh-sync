use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const BASE36_WIDTH: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdConfig {
    pub min_len: usize,
    pub max_len: usize,
    pub max_collision: f64,
}

impl Default for IdConfig {
    fn default() -> Self {
        Self {
            min_len: 3,
            max_len: 8,
            max_collision: 0.25,
        }
    }
}

pub fn adaptive_len(beads: usize, config: &IdConfig) -> usize {
    let n = beads as f64;
    (config.min_len..=config.max_len)
        .find(|&len| {
            let space = 36f64.powi(len as i32);
            1.0 - (-(n * (n - 1.0)) / (2.0 * space)).exp() <= config.max_collision
        })
        .unwrap_or(config.max_len)
}

fn suffix(key: &str, len: usize) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut value = u128::from_be_bytes(digest[..16].try_into().expect("16 bytes"));
    let mut digits = [b'0'; BASE36_WIDTH];
    for slot in digits.iter_mut().rev() {
        *slot = DIGITS[(value % 36) as usize];
        value /= 36;
    }
    let len = len.min(BASE36_WIDTH);
    String::from_utf8_lossy(&digits[BASE36_WIDTH - len..]).into_owned()
}

pub fn short_id(
    prefix: &str,
    key: &str,
    len: usize,
    config: &IdConfig,
    taken: &BTreeSet<String>,
) -> String {
    for len in len..=config.max_len.max(len) {
        let id = format!("{prefix}-{}", suffix(key, len));
        if !taken.contains(&id) {
            return id;
        }
    }
    (2..)
        .map(|i| format!("{prefix}-{}{i}", suffix(key, config.max_len)))
        .find(|id| !taken.contains(id))
        .expect("unbounded range")
}

pub fn imported_prefix(id: &str) -> Option<&str> {
    let (rest, hash) = id.rsplit_once('-')?;
    let (rest, counter) = rest.rsplit_once('-')?;
    let (prefix, millis) = rest.rsplit_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (!prefix.is_empty()
        && hash.len() == 8
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && digits(counter)
        && digits(millis)
        && millis.len() >= 10)
        .then_some(prefix)
}

pub fn next_child(parent: &str, ids: &BTreeSet<String>) -> String {
    let last = ids
        .iter()
        .filter_map(|id| id.strip_prefix(parent)?.strip_prefix('.'))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{parent}.{}", last + 1)
}

pub fn descendants(id: &str, ids: &BTreeSet<String>) -> Vec<String> {
    let prefix = format!("{id}.");
    let mut found: Vec<String> = ids
        .iter()
        .filter(|other| other.starts_with(&prefix))
        .cloned()
        .collect();
    found.sort_by_key(|other| other.len());
    found
}

pub fn rename_ids(value: &mut Value, renames: &BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut inner) in entries {
                rename_ids(&mut inner, renames);
                map.insert(renames.get(&key).cloned().unwrap_or(key), inner);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| rename_ids(item, renames)),
        Value::String(text) => {
            if let Some(new) = renames.get(text.as_str()) {
                *text = new.clone();
                return;
            }
            let parts: Vec<&str> = text.split(' ').collect();
            if parts.len() == 3
                && (renames.contains_key(parts[0]) || renames.contains_key(parts[2]))
            {
                let swap = |id: &str| renames.get(id).map_or(id, String::as_str).to_string();
                *text = format!("{} {} {}", swap(parts[0]), parts[1], swap(parts[2]));
            }
        }
        _ => {}
    }
}
