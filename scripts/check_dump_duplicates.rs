#!/usr/bin/env -S cargo +nightly -Zscript
//! Scans a JSONL state dump for duplicate addresses and duplicate storage keys
//! within a single account. Streams line-by-line to handle large dumps.
//!
//! Usage: cargo +nightly -Zscript scripts/check_dump_duplicates.rs <dump.jsonl>

use std::{
    collections::HashSet,
    env,
    fs::File,
    io::{BufRead, BufReader},
    process,
};

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: check_dump_duplicates <dump.jsonl>");
        process::exit(1);
    });

    let file = File::open(&path).unwrap_or_else(|e| {
        eprintln!("Failed to open {path}: {e}");
        process::exit(1);
    });

    let reader = BufReader::with_capacity(8 * 1024 * 1024, file);
    let mut seen_addresses: HashSet<Box<[u8]>> = HashSet::with_capacity(25_000_000);
    let mut dup_addr_count: usize = 0;
    let mut dup_slot_count: usize = 0;
    let mut line_num: usize = 0;
    let mut account_count: usize = 0;

    for line_result in reader.lines() {
        line_num += 1;
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Line {line_num}: read error: {e}");
                continue;
            }
        };

        if line_num == 1 {
            continue; // skip state root header
        }

        let address = match extract_address(&line) {
            Some(a) => a,
            None => {
                eprintln!("Line {line_num}: missing address");
                continue;
            }
        };

        account_count += 1;
        if account_count % 5_000_000 == 0 {
            eprintln!("Processed {account_count} accounts...");
        }

        if !seen_addresses.insert(address.as_bytes().into()) {
            eprintln!("Duplicate address at line {line_num}: {address}");
            dup_addr_count += 1;
        }

        dup_slot_count += check_duplicate_storage_keys(&line, line_num, &address);
    }

    eprintln!("Scanned {account_count} accounts across {line_num} lines.");

    if dup_addr_count == 0 && dup_slot_count == 0 {
        println!("No duplicates found.");
    } else {
        println!("{dup_addr_count} duplicate address(es), {dup_slot_count} duplicate storage key(s).");
        process::exit(1);
    }
}

fn extract_address(line: &str) -> Option<String> {
    let marker = "\"address\"";
    let pos = line.find(marker)?;
    let after = &line[pos + marker.len()..];
    let quote1 = after.find('"')? + 1;
    let quote2 = after[quote1..].find('"')?;
    Some(after[quote1..quote1 + quote2].to_lowercase())
}

/// Scan raw JSON for duplicate storage keys. Counts each "0x...": occurrence
/// inside the "storage":{...} block.
fn check_duplicate_storage_keys(line: &str, line_num: usize, address: &str) -> usize {
    let storage_start = match line.find("\"storage\"") {
        Some(pos) => pos,
        None => return 0,
    };
    let rest = &line[storage_start..];
    let brace_start = match rest.find('{') {
        Some(pos) => pos,
        None => return 0,
    };
    let storage_str = &rest[brace_start..];

    let mut seen: HashSet<&str> = HashSet::new();
    let mut duplicates: usize = 0;
    let bytes = storage_str.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        if bytes[pos] == b'"' {
            let key_start = pos + 1;
            let key_end = match memchr(b'"', &bytes[key_start..]) {
                Some(offset) => key_start + offset,
                None => break,
            };
            // Check if followed by ':' (key, not value)
            let mut peek = key_end + 1;
            while peek < bytes.len() && bytes[peek] == b' ' {
                peek += 1;
            }
            if peek < bytes.len() && bytes[peek] == b':' {
                let key = &storage_str[key_start..key_end];
                if !seen.insert(key) {
                    eprintln!("Duplicate storage key at line {line_num}: {address} slot {key}");
                    duplicates += 1;
                }
            }
            pos = key_end + 1;
        } else if bytes[pos] == b'}' {
            break;
        } else {
            pos += 1;
        }
    }
    duplicates
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}
