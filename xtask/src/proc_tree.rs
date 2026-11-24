//! Resident memory of a process and its descendants, by process name, from
//! `/proc` (Linux only).

use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;

use crate::bench_rss::proc_status_kib;

/// `(pid, parent pid, name)` of every process readable in `/proc`.
fn processes() -> Result<Vec<(u32, u32, String)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Some(pid) = entry?.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let name = stat[open + 1..close].to_owned();
        let Some(ppid) = stat[close + 1..]
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse().ok())
        else {
            continue;
        };
        out.push((pid, ppid, name));
    }
    Ok(out)
}

/// `root` and every descendant, with their names.
pub fn tree(root: u32) -> Result<Vec<(u32, String)>> {
    let all = processes()?;
    let mut tree = vec![root];
    let mut i = 0;
    while let Some(&parent) = tree.get(i) {
        tree.extend(all.iter().filter(|p| p.1 == parent).map(|p| p.0));
        i += 1;
    }
    Ok(all
        .into_iter()
        .filter(|p| tree.contains(&p.0))
        .map(|(pid, _, name)| (pid, name))
        .collect())
}

/// VmRSS in KiB of `root` and every descendant, summed per process name.
pub fn tree_rss_kib(root: u32) -> Result<BTreeMap<String, u64>> {
    let mut by_name = BTreeMap::new();
    for (pid, name) in tree(root)? {
        // Exited since the listing, or a kernel thread without VmRSS.
        if let Ok(kib) = proc_status_kib(pid, "VmRSS") {
            *by_name.entry(name).or_insert(0) += kib;
        }
    }
    Ok(by_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn sums_children_by_name() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 5 & sleep 5 & wait"])
            .spawn()
            .unwrap();
        let mut sleeps = 0;
        for _ in 0..50 {
            let tree = tree_rss_kib(std::process::id()).unwrap();
            sleeps = tree.get("sleep").copied().unwrap_or(0);
            if sleeps > 0 && tree.contains_key("sh") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(sleeps > 0);
    }
}
