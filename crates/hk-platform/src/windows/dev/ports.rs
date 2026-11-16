//! Which programs are waiting for connections on which ports, like a dev server on 3000.

use std::net::{Ipv4Addr, Ipv6Addr};

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
    MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

pub(super) struct Listening {
    pub(super) port: u16,
    pub(super) address: String,
    pub(super) pid: u32,
}

/// Every TCP port something is listening on, lowest first.
pub(super) fn listening() -> Vec<Listening> {
    let mut found = Vec::new();
    if let Some(table) = table(u32::from(AF_INET.0)) {
        // SAFETY: Windows wrote a table of this kind into the buffer, saying how many rows it has.
        let rows = unsafe {
            let table = &*table.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
            std::slice::from_raw_parts::<MIB_TCPROW_OWNER_PID>(
                table.table.as_ptr(),
                table.dwNumEntries as usize,
            )
        };
        found.extend(rows.iter().map(|row| Listening {
            port: port(row.dwLocalPort),
            address: Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).to_string(),
            pid: row.dwOwningPid,
        }));
    }
    if let Some(table) = table(u32::from(AF_INET6.0)) {
        // SAFETY: as above.
        let rows = unsafe {
            let table = &*table.as_ptr().cast::<MIB_TCP6TABLE_OWNER_PID>();
            std::slice::from_raw_parts::<MIB_TCP6ROW_OWNER_PID>(
                table.table.as_ptr(),
                table.dwNumEntries as usize,
            )
        };
        found.extend(rows.iter().map(|row| Listening {
            port: port(row.dwLocalPort),
            address: Ipv6Addr::from(row.ucLocalAddr).to_string(),
            pid: row.dwOwningPid,
        }));
    }
    found.sort_by(|a, b| a.port.cmp(&b.port).then(a.address.cmp(&b.address)));
    found.dedup_by(|a, b| a.port == b.port && a.address == b.address && a.pid == b.pid);
    found
}

/// Kept in the low two bytes, in network order.
fn port(raw: u32) -> u16 {
    u16::from_be((raw & 0xFFFF) as u16)
}

/// The table for one kind of address, in a buffer of u64s so that it's aligned.
fn table(family: u32) -> Option<Vec<u64>> {
    let mut size = 0u32;
    let mut buffer: Vec<u64> = Vec::new();
    // The table can grow between asking its size and reading it.
    for _ in 0..4 {
        // SAFETY: the buffer is at least `size` bytes, which Windows is told.
        let result = unsafe {
            GetExtendedTcpTable(
                (!buffer.is_empty()).then(|| buffer.as_mut_ptr().cast()),
                &raw mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if result == NO_ERROR.0 && !buffer.is_empty() {
            return Some(buffer);
        }
        if result != ERROR_INSUFFICIENT_BUFFER.0 && result != NO_ERROR.0 {
            return None;
        }
        buffer = vec![0u64; (size as usize).div_ceil(8) + 1];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_read_from_network_order() {
        // Port 3000 is 0x0BB8, stored as B8 0B... the other way round, in the low bytes.
        assert_eq!(port(u32::from(3000_u16.to_be())), 3000);
        assert_eq!(port(0xFFFF_0000 | u32::from(443_u16.to_be())), 443);
    }

    /// Only looks. Something is always listening on a Windows machine.
    #[test]
    fn this_machine_has_ports_in_use() {
        let ports = listening();
        assert!(!ports.is_empty());
        assert!(ports.windows(2).all(|pair| pair[0].port <= pair[1].port));
    }
}
