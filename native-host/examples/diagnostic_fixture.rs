//! Replay synthetic numeric evidence through the production reporting worker.
use holodori_native_host::{diagnostics::Event, metrics::HostMetrics};
use std::{error::Error, fs, path::Path};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    let input = args.get(1).ok_or("input CSV required")?;
    let output = args.get(2).ok_or("output report path required")?;
    let mut metrics = HostMetrics::new(true, 8.333333, 5);
    metrics.configure(
        "synthetic-wifi",
        "synthetic fixture; no physical latency claim; QoS/thermal/driver unavailable".into(),
    );
    for line in fs::read_to_string(input)?.lines() {
        let values: Vec<u64> = line.split(',').map(str::parse).collect::<Result<_, _>>()?;
        let values: [u64; 12] = values
            .try_into()
            .map_err(|_| "expected 12 numeric columns")?;
        metrics.record(Event(values));
    }
    metrics.write_report(Path::new(output))?;
    Ok(())
}
