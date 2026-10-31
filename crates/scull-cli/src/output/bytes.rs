//! Byte formatting with explicit size labels (PHY-01: every size shown is labeled logical,
//! allocated or reclaimable).

use scull_core::{Confidence, SizeKind};

/// Format a byte count with binary (IEC) units: `0 B`, `999 B`, `1.0 KiB`, `12.3 MiB`.
///
/// One decimal place above 1 KiB; exact bytes below. Never rounds up to the next unit's
/// boundary in a misleading way (e.g. 1023 B stays `1023 B`).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // Promote when rounding to one decimal would display 1024.0.
    while value >= 1023.95 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Column header for a size kind, e.g. `Logical`, `Allocated`, `Reclaimable` (PHY-01).
pub fn size_header(kind: SizeKind) -> &'static str {
    match kind {
        SizeKind::Logical => "Logical",
        SizeKind::Allocated => "Allocated",
        SizeKind::Reclaimable => "Reclaimable",
        SizeKind::Measured => "Measured",
    }
}

/// A byte count with its label inline, for prose output: `12.3 MiB reclaimable`.
pub fn labeled_bytes(bytes: u64, kind: SizeKind) -> String {
    format!(
        "{} {}",
        format_bytes(bytes),
        size_header(kind).to_lowercase()
    )
}

/// Short confidence marker shown next to reclaimable sizes (PRD §9).
pub fn confidence_label(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
        Confidence::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        let cases = [
            (0, "0 B"),
            (1, "1 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1024 * 1024 - 1, "1.0 MiB"),
            (10 * 1024 * 1024 + 300 * 1024, "10.3 MiB"),
            (5 * 1024u64.pow(3), "5.0 GiB"),
            (3 * 1024u64.pow(4), "3.0 TiB"),
            (u64::MAX, "16.0 EiB"),
        ];
        for (n, want) in cases {
            assert_eq!(format_bytes(n), want, "{n}");
        }
    }

    #[test]
    fn every_size_kind_has_a_label() {
        assert_eq!(size_header(SizeKind::Logical), "Logical");
        assert_eq!(size_header(SizeKind::Allocated), "Allocated");
        assert_eq!(size_header(SizeKind::Reclaimable), "Reclaimable");
        assert_eq!(size_header(SizeKind::Measured), "Measured");
        assert_eq!(
            labeled_bytes(2048, SizeKind::Reclaimable),
            "2.0 KiB reclaimable"
        );
        assert_eq!(confidence_label(Confidence::Unknown), "unknown");
    }
}
