//! Finding the TCP ports a dev server actually listens on.
//!
//! `forkspace run` passes `$PORT`, but many dev servers ignore it (Vite listens on 5173 unless
//! told otherwise). The server is started as the leader of its own session, so every process
//! it starts shares that session (Linux) or process group (macOS) unless it detaches itself.

use std::collections::BTreeSet;

/// TCP ports in LISTEN state owned by processes in `leader`'s session, sorted ascending.
#[cfg(target_os = "linux")]
pub(crate) fn listening_ports(leader: u32) -> Vec<u16> {
    use std::collections::HashSet;
    use std::fs;

    let mut socket_inodes = HashSet::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if session_of(pid) != Some(leader) {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            let Ok(target) = fs::read_link(descriptor.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            if let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|inode| inode.parse::<u64>().ok())
            {
                socket_inodes.insert(inode);
            }
        }
    }

    let mut ports = BTreeSet::new();
    for table in ["tcp", "tcp6"] {
        let Ok(contents) = fs::read_to_string(format!("/proc/{leader}/net/{table}")) else {
            continue;
        };
        ports.extend(
            listening_entries(&contents)
                .filter_map(|(port, inode)| socket_inodes.contains(&inode).then_some(port)),
        );
    }
    ports.into_iter().collect()
}

/// The session ID of `pid`, from field 6 of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn session_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesized and may contain spaces; fields resume after it.
    let fields = &stat[stat.rfind(')')? + 1..];
    fields.split_whitespace().nth(3)?.parse().ok()
}

/// `(port, socket inode)` for each LISTEN row of a `/proc/net/tcp{,6}` table.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn listening_entries(table: &str) -> impl Iterator<Item = (u16, u64)> + '_ {
    const TCP_LISTEN: &str = "0A";
    table.lines().skip(1).filter_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(3) != Some(&TCP_LISTEN) {
            return None;
        }
        let port = fields.get(1)?.rsplit(':').next()?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let inode = fields.get(9)?.parse().ok()?;
        Some((port, inode))
    })
}

/// TCP ports in LISTEN state owned by processes in `leader`'s process group, sorted ascending.
#[cfg(target_os = "macos")]
pub(crate) fn listening_ports(leader: u32) -> Vec<u16> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fn", "-g"])
        .arg(leader.to_string())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter_map(|address| address.rsplit(':').next()?.parse().ok())
        .collect::<BTreeSet<u16>>()
        .into_iter()
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn listening_ports(_leader: u32) -> Vec<u16> {
    Vec::new()
}

/// The port to route to among `listening`: the assigned `$PORT` when the server honored it,
/// otherwise the lowest port it listens on.
pub(crate) fn pick_port(listening: &[u16], assigned: u16) -> Option<u16> {
    if listening.contains(&assigned) {
        return Some(assigned);
    }
    listening.first().copied()
}

#[cfg(test)]
mod tests {
    use super::{listening_entries, pick_port};

    #[test]
    fn listen_rows_are_parsed_and_other_states_skipped() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0 100 0 0 10 0\n\
   1: 0100007F:0BB8 0100007F:D2A4 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0 20 4 30 10 -1\n";
        let rows: Vec<_> = listening_entries(table).collect();
        assert_eq!(rows, vec![(0x1435, 4242)]);
    }

    #[test]
    fn assigned_port_wins_otherwise_lowest() {
        assert_eq!(pick_port(&[3000, 5173], 5173), Some(5173));
        assert_eq!(pick_port(&[5173, 24678], 3000), Some(5173));
        assert_eq!(pick_port(&[], 3000), None);
    }
}
