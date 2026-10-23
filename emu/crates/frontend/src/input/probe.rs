//! `input-probe`: a windowless look at what the input layer receives.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::model::rising_edges;
use super::InputRouter;

/// Watch the input layer for `seconds`, printing hot-plug notices as they
/// happen and each device's live state twice a second.
pub(crate) fn run(seconds: u64) -> Result<(), String> {
    let mut router = InputRouter::new();
    let start = Instant::now();
    let mut next_report = Duration::ZERO;
    let mut last_line: BTreeMap<String, String> = BTreeMap::new();
    let mut presses: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut previous: BTreeMap<String, u16> = BTreeMap::new();
    println!("input-probe: watching for {seconds} s. Press buttons on each device.");
    while start.elapsed() < Duration::from_secs(seconds) {
        pump_platform_events();
        let frame = router.poll();
        for notice in &frame.notices {
            println!(
                "{:>6.2}s {}",
                start.elapsed().as_secs_f32(),
                notice.log_line()
            );
        }
        for row in router.devices() {
            let before = previous
                .insert(row.identity.id.clone(), row.live.buttons)
                .unwrap_or(0);
            let entry = presses
                .entry(row.identity.id.clone())
                .or_insert_with(|| (row.identity.name.clone(), 0));
            entry.1 += rising_edges(before, row.live.buttons).count_ones() as usize;
            let line = format!(
                "{} -> {}{}: {}",
                row.identity.name,
                row.port.label(),
                if row.ignored { " [IGNORED]" } else { "" },
                row.live.describe()
            );
            if start.elapsed() >= next_report || last_line.get(&row.identity.id) != Some(&line) {
                if last_line.get(&row.identity.id) != Some(&line) {
                    println!("{:>6.2}s {line}", start.elapsed().as_secs_f32());
                }
                last_line.insert(row.identity.id.clone(), line);
            }
        }
        if start.elapsed() >= next_report {
            next_report = start.elapsed() + Duration::from_millis(500);
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    println!("input-probe: button presses seen per device");
    for (name, count) in presses.values() {
        println!("  {name}: {count}");
    }
    Ok(())
}

/// Let the platform deliver device notifications on this thread.
fn pump_platform_events() {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSDate, NSRunLoop};
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.001));
    }
}
