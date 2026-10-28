use std::time::Duration;

pub const FRAME: Duration = Duration::from_millis(125);
pub const FLASH: Duration = Duration::from_millis(700);

const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
const FLOW: [&str; 3] = ["•··", "·•·", "··•"];
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn spinner(frame: u64) -> &'static str {
    SPINNER[(frame / 2 % SPINNER.len() as u64) as usize]
}

/// `true` on the row of a wire of `period` rows that the pulse sits on in this frame.
pub fn pulse(frame: u64, ordinal: usize, period: usize) -> bool {
    period > 0 && ordinal as u64 == frame / 2 % period as u64
}

pub fn flow(frame: u64) -> &'static str {
    FLOW[(frame % FLOW.len() as u64) as usize]
}

/// One bar per bucket, scaled to the largest and rounded up, so a single call still rises
/// above an empty minute.
pub fn sparkline(buckets: &[u16]) -> String {
    let max = buckets.iter().copied().max().unwrap_or(0).max(1) as usize;
    let top = BARS.len() - 1;
    buckets
        .iter()
        .map(|&n| BARS[(n as usize * top).div_ceil(max)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spinner_steps_every_second_frame_and_cycles() {
        let seq: Vec<&str> = (0..10).map(spinner).collect();
        assert_eq!(seq, vec!["◐", "◐", "◓", "◓", "◑", "◑", "◒", "◒", "◐", "◐"]);
    }

    #[test]
    fn the_pulse_moves_one_row_every_second_frame_and_wraps() {
        let at = |frame| (0..3).position(|o| pulse(frame, o, 3));
        assert_eq!(
            (0..8).map(at).collect::<Vec<_>>(),
            [0, 0, 1, 1, 2, 2, 0, 0].map(Some).to_vec()
        );
        assert!(!pulse(0, 0, 0));
    }

    #[test]
    fn flow_steps_every_frame() {
        assert_eq!(
            (0..4).map(flow).collect::<Vec<_>>(),
            vec!["•··", "·•·", "··•", "•··"]
        );
    }

    #[test]
    fn the_sparkline_scales_to_the_busiest_bucket() {
        assert_eq!(sparkline(&[0, 1, 2, 4, 8]), "▁▂▃▅█");
        assert_eq!(sparkline(&[3, 3]), "██");
        assert_eq!(sparkline(&[0; 10]), "▁".repeat(10));
    }
}
