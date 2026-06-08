//! Adapter crate for OpenDeck integration.
//! Minimal glue: expose a function that accepts a device report and returns indices of keys that are pressed.

pub fn handle_report(report: &[u8]) -> Vec<usize> {
    let events = driver::parse_report(report);
    // Return indices where pressed == true
    events
        .into_iter()
        .filter_map(|e| if e.pressed { Some(e.index) } else { None })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_handles_report() {
        // craft a report matching driver mapping: report[0]=0x01, report[42]=0x02 -> key 0 pressed
        let mut report = [0u8; 64];
        report[0] = 0x01;
        report[42] = 0x02;
        let res = handle_report(&report);
        assert_eq!(res, vec![0usize]);
    }
}
