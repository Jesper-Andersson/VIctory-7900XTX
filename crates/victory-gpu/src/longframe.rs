//! Long frames one by one (`VICTORY_LONG_FRAMES=<ms>`, off by default, read
//! once): a line for every present that came at least that long after the
//! one before, saying where that frame's time went. The lines of every 5 s
//! average many frames, and a 1.5 s frame among fifty of 95 ms hardly moves
//! them: they can tell which stretches stuttered, not what held any one
//! frame.
//!
//! Every number in the line is for that one frame, from the present before
//! it to this one: from the present before it until the title's first list
//! call and its first submission, and from its last submission to this
//! present (time the title spent on its own side is the first of these); the
//! list calls' time over all recording threads, the waits for pipelines
//! being made and the title's frame waits; our copies of title memory (time
//! and bytes), the refreshes of copies (how many, the lengths of the ranges
//! named, which are not bytes copied: most of a named range is found
//! unchanged by the write watch; then what the watch checked and found
//! changed, and what was copied whole because its memory is not watched),
//! our steps run on the host, texture uploads recorded, the card waited for,
//! what shaders wrote put back, and the waits for new memory to be resident
//! and mapped (`VICTORY_WAIT_RESIDENT`, #371); the process's page faults and
//! processor time (user, kernel, all threads). Only the PC's free memory and
//! the pipelines being made are as they are at the present.
//!
//! `1` or `on`: 250 ms. A number: that many ms, at least 50 (so that a
//! steady 95 ms frame rate does not log every frame). Diagnostics builds
//! only (the counters are `copystat`'s, read as running totals and diffed);
//! with it off nothing is asked at a present. The counters of other threads
//! are read as they are at the present, so work that straddles it is counted
//! in the frame it ends in.

use std::sync::OnceLock;

/// The shortest frame logged, ms, or `None` (off).
pub(crate) fn threshold() -> Option<f64> {
    static AT: OnceLock<Option<f64>> = OnceLock::new();
    *AT.get_or_init(|| {
        let at = threshold_of(std::env::var("VICTORY_LONG_FRAMES").ok().as_deref().unwrap_or(""));
        if let Some(ms) = at {
            tracing::info!(ms, "long frames are logged one by one (VICTORY_LONG_FRAMES)");
        }
        at
    })
}

/// The switch's value as a threshold in ms.
pub(crate) fn threshold_of(value: &str) -> Option<f64> {
    match value.trim() {
        "" | "0" | "off" => None,
        "1" | "on" => Some(250.0),
        v => v.parse::<f64>().ok().filter(|ms| ms.is_finite() && *ms > 0.0).map(|ms| ms.max(50.0)),
    }
}

/// Running totals since the start, at a present (microseconds and bytes
/// unless named otherwise). [`Sample::since`] of two makes a frame's.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Sample {
    /// When the present was counted.
    pub(crate) at_us: u64,
    pub(crate) until_record_us: u64,
    pub(crate) until_submit_us: u64,
    pub(crate) tail_us: u64,
    pub(crate) record_ns: u64,
    pub(crate) pipeline_wait_us: u64,
    pub(crate) pace_us: u64,
    /// Title copies (copy and fill steps): time, bytes.
    pub(crate) copy_us: u64,
    pub(crate) copy_bytes: u64,
    /// Our steps run on the host (`Step::Run`, `Step::Host`).
    pub(crate) steps_us: u64,
    /// Refreshes, and the summed lengths of the ranges they name.
    pub(crate) refreshes: u64,
    pub(crate) refresh_named_bytes: u64,
    /// Watched memory checked, of it found unchanged; memory copied whole.
    pub(crate) checked_bytes: u64,
    pub(crate) unchanged_bytes: u64,
    pub(crate) whole_bytes: u64,
    pub(crate) texture_uploads: u64,
    pub(crate) card_wait_us: u64,
    pub(crate) back_us: u64,
    /// Waits for new memory (`host::resident`, `host::mapped`), all threads.
    pub(crate) resident_us: u64,
    pub(crate) mapped_us: u64,
    pub(crate) page_faults: u64,
    pub(crate) user_us: u64,
    pub(crate) kernel_us: u64,
}

impl Sample {
    /// What happened from `before` to `self`. Totals only grow; one that
    /// went down anyway counts as nothing rather than wrapping.
    pub(crate) fn since(&self, before: &Sample) -> Sample {
        let d = |a: u64, b: u64| a.saturating_sub(b);
        Sample {
            at_us: d(self.at_us, before.at_us),
            until_record_us: d(self.until_record_us, before.until_record_us),
            until_submit_us: d(self.until_submit_us, before.until_submit_us),
            tail_us: d(self.tail_us, before.tail_us),
            record_ns: d(self.record_ns, before.record_ns),
            pipeline_wait_us: d(self.pipeline_wait_us, before.pipeline_wait_us),
            pace_us: d(self.pace_us, before.pace_us),
            copy_us: d(self.copy_us, before.copy_us),
            copy_bytes: d(self.copy_bytes, before.copy_bytes),
            steps_us: d(self.steps_us, before.steps_us),
            refreshes: d(self.refreshes, before.refreshes),
            refresh_named_bytes: d(self.refresh_named_bytes, before.refresh_named_bytes),
            checked_bytes: d(self.checked_bytes, before.checked_bytes),
            unchanged_bytes: d(self.unchanged_bytes, before.unchanged_bytes),
            whole_bytes: d(self.whole_bytes, before.whole_bytes),
            texture_uploads: d(self.texture_uploads, before.texture_uploads),
            card_wait_us: d(self.card_wait_us, before.card_wait_us),
            back_us: d(self.back_us, before.back_us),
            resident_us: d(self.resident_us, before.resident_us),
            mapped_us: d(self.mapped_us, before.mapped_us),
            page_faults: d(self.page_faults, before.page_faults),
            user_us: d(self.user_us, before.user_us),
            kernel_us: d(self.kernel_us, before.kernel_us),
        }
    }
}

/// The line for one frame (`f`: [`Sample::since`] of its two presents),
/// if it is at least `threshold` ms long.
pub(crate) fn line(f: &Sample, threshold: f64, free_mb: Option<u64>, being_made: u64) -> Option<String> {
    let ms = |us: u64| us as f64 / 1e3;
    let mb = |bytes: u64| bytes as f64 / 1e6;
    let frame = ms(f.at_us);
    if frame < threshold {
        return None;
    }
    let free = free_mb.map_or_else(|| "?".to_string(), |mb| mb.to_string());
    Some(format!(
        "long frame (VICTORY_LONG_FRAMES={threshold:.0}), all for this frame alone: {frame:.0} ms; after the present \
         before it {:.1} ms until the title's first list call, {:.1} ms until its first submission, {:.1} ms from its \
         last submission to this present; list calls {:.1} ms (all threads), waiting for pipelines {:.1} ms, frame \
         waits slept {:.1} ms; title copies {:.1} ms {:.1} MB; {} refreshes naming {:.1} MB of ranges (lengths, not \
         bytes copied), the write watch checked {:.1} MB and found {:.1} MB changed, {:.1} MB copied whole (memory not \
         watched); our steps on the host {:.1} ms; {} texture uploads; the card waited for {:.1} ms; what shaders wrote \
         put back {:.1} ms; new memory waited for {:.1} ms resident + {:.1} ms mapped (all threads, \
         VICTORY_WAIT_RESIDENT); {} page faults; the process ran {:.0} ms user + {:.0} ms kernel; at the present: the PC \
         has {free} MB free, {being_made} pipelines being made",
        ms(f.until_record_us),
        ms(f.until_submit_us),
        ms(f.tail_us),
        f.record_ns as f64 / 1e6,
        ms(f.pipeline_wait_us),
        ms(f.pace_us),
        ms(f.copy_us),
        mb(f.copy_bytes),
        f.refreshes,
        mb(f.refresh_named_bytes),
        mb(f.checked_bytes),
        mb(f.checked_bytes.saturating_sub(f.unchanged_bytes)),
        mb(f.whole_bytes),
        ms(f.steps_us),
        f.texture_uploads,
        ms(f.card_wait_us),
        ms(f.back_us),
        ms(f.resident_us),
        ms(f.mapped_us),
        f.page_faults,
        ms(f.user_us),
        ms(f.kernel_us),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_as_a_threshold() {
        assert_eq!(threshold_of(""), None);
        assert_eq!(threshold_of("0"), None);
        assert_eq!(threshold_of("off"), None);
        assert_eq!(threshold_of("1"), Some(250.0));
        assert_eq!(threshold_of(" on "), Some(250.0));
        assert_eq!(threshold_of("400"), Some(400.0));
        assert_eq!(threshold_of("10"), Some(50.0));
        assert_eq!(threshold_of("-5"), None);
        assert_eq!(threshold_of("inf"), None);
        assert_eq!(threshold_of("x"), None);
    }

    #[test]
    fn a_frame_is_the_difference_of_its_presents_and_never_negative() {
        let before = Sample { at_us: 1_000, until_record_us: 500, page_faults: 10, user_us: 7, ..Default::default() };
        let now = Sample { at_us: 401_000, until_record_us: 300_500, page_faults: 12_010, user_us: 2, ..Default::default() };
        let f = now.since(&before);
        assert_eq!(f.at_us, 400_000);
        assert_eq!(f.until_record_us, 300_000);
        assert_eq!(f.page_faults, 12_000);
        assert_eq!(f.user_us, 0);
    }

    /// Every field of a frame is its own delta: three presents with growing
    /// totals give two frames whose fields add up to the whole, none of them
    /// a total since the start.
    #[test]
    fn every_field_of_a_frame_is_that_frame_alone() {
        let grow = |k: u64| Sample {
            at_us: 1_000_000 * k,
            until_record_us: 10 * k,
            until_submit_us: 11 * k,
            tail_us: 12 * k,
            record_ns: 13 * k,
            pipeline_wait_us: 14 * k,
            pace_us: 15 * k,
            copy_us: 16 * k,
            copy_bytes: 17 * k,
            steps_us: 18 * k,
            refreshes: 19 * k,
            refresh_named_bytes: 18_000_000_000 * k,
            checked_bytes: 21 * k,
            unchanged_bytes: 20 * k,
            whole_bytes: 23 * k,
            texture_uploads: 24 * k,
            card_wait_us: 25 * k,
            back_us: 26 * k,
            resident_us: 27 * k,
            mapped_us: 28 * k,
            page_faults: 29 * k,
            user_us: 30 * k,
            kernel_us: 31 * k,
        };
        let (a, b, c) = (grow(5), grow(6), grow(7));
        let (first, second) = (b.since(&a), c.since(&b));
        assert_eq!(first, grow(1));
        assert_eq!(second, grow(1));
        // A running total since the start would be grow(6) here.
        assert_eq!(b.since(&Sample::default()), grow(6));
    }

    #[test]
    fn only_frames_past_the_threshold_make_a_line() {
        let short = Sample { at_us: 200_000, ..Default::default() };
        assert!(line(&short, 250.0, Some(3000), 0).is_none());
        let long = Sample {
            at_us: 1_230_000,
            until_record_us: 812_000,
            pipeline_wait_us: 4_000,
            copy_us: 2_500,
            copy_bytes: 3_000_000,
            refreshes: 900,
            refresh_named_bytes: 12_000_000_000,
            checked_bytes: 8_000_000_000,
            unchanged_bytes: 7_900_000_000,
            whole_bytes: 5_000_000,
            steps_us: 700,
            page_faults: 54_000,
            kernel_us: 401_000,
            resident_us: 205_300,
            ..Default::default()
        };
        let text = line(&long, 250.0, Some(912), 3).expect("a long frame");
        assert!(text.contains("all for this frame alone: 1230 ms;"), "{text}");
        assert!(text.contains("812.0 ms until the title's first list call"), "{text}");
        assert!(text.contains("waiting for pipelines 4.0 ms"), "{text}");
        assert!(text.contains("title copies 2.5 ms 3.0 MB"), "{text}");
        assert!(text.contains("900 refreshes naming 12000.0 MB of ranges (lengths, not bytes copied)"), "{text}");
        assert!(text.contains("checked 8000.0 MB and found 100.0 MB changed, 5.0 MB copied whole"), "{text}");
        assert!(text.contains("our steps on the host 0.7 ms"), "{text}");
        assert!(text.contains("new memory waited for 205.3 ms resident + 0.0 ms mapped"), "{text}");
        assert!(text.contains("54000 page faults"), "{text}");
        assert!(text.contains("401 ms kernel"), "{text}");
        assert!(text.contains("912 MB free, 3 pipelines being made"), "{text}");
        assert!(line(&long, 250.0, None, 0).expect("a long frame").contains("the PC has ? MB free"));
    }
}
