//! Responsive width measurement, spacer group splitting, and staged compaction.

use crate::sys::fmt::truncate_to_width;

use super::emit::StyleGeometry;
use super::{CompactStage, Segment};

const COMPACT_STAGES: &[CompactStage] = &[
    CompactStage::VcsDropDiff,
    CompactStage::VcsTruncate,
    CompactStage::SubagentsDropLabel,
    CompactStage::CostStripSuffix,
    CompactStage::SpeedStripRetry,
    CompactStage::QuotaStripReset,
    CompactStage::CtxDropLimit,
    CompactStage::CtxCollapseBar,
    CompactStage::ModelAbbreviate,
    CompactStage::ModelTruncate,
    CompactStage::AccountDropDomain,
    CompactStage::TitleTruncate,
];

fn groups_visible_width(groups: &[Vec<Segment>], geom: StyleGeometry) -> usize {
    if groups.is_empty() {
        return 0;
    }
    let sum: usize = groups.iter().map(|g| geom.slice_visible_width(g)).sum();
    sum + groups.len().saturating_sub(1)
}

pub(super) fn split_into_groups(segs: Vec<Segment>) -> Vec<Vec<Segment>> {
    let mut groups: Vec<Vec<Segment>> = Vec::new();
    let mut cur = Vec::new();
    for seg in segs {
        if seg.is_spacer {
            if !cur.is_empty() {
                groups.push(cur);
                cur = Vec::new();
            }
        } else {
            cur.push(seg);
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    while groups.len() > 3 {
        let tail = groups.remove(3);
        groups[2].extend(tail);
    }
    groups
}

pub(super) fn fit_row_groups(
    mut groups: Vec<Vec<Segment>>,
    max_width: Option<usize>,
    geom: StyleGeometry,
    ellipsis: &str,
) -> Vec<Vec<Segment>> {
    let Some(limit) = max_width.filter(|&w| w > 0) else {
        return groups;
    };
    if groups_visible_width(&groups, geom) <= limit {
        return groups;
    }

    // Apply structured per-segment compaction fallbacks in priority order.
    for &stage in COMPACT_STAGES {
        for group in &mut groups {
            for seg in group {
                seg.apply_compaction(stage);
            }
        }
        if groups_visible_width(&groups, geom) <= limit {
            return groups;
        }
    }

    // Priority-based widget drop and final single-segment truncation.
    while groups_visible_width(&groups, geom) > limit {
        let total_segs: usize = groups.iter().map(Vec::len).sum();
        if total_segs <= 1 {
            break;
        }
        let mut drop_pos: Option<(usize, usize, u8)> = None;
        for (gi, g) in groups.iter().enumerate() {
            for (si, seg) in g.iter().enumerate() {
                let p = seg.widget.priority();
                let replace = match drop_pos {
                    None => true,
                    Some((_, _, best_p)) => p <= best_p,
                };
                if replace {
                    drop_pos = Some((gi, si, p));
                }
            }
        }
        if let Some((gi, si, _)) = drop_pos {
            groups[gi].remove(si);
            if groups[gi].is_empty() {
                groups.remove(gi);
            }
        } else {
            break;
        }
    }

    if groups_visible_width(&groups, geom) > limit && groups.len() == 1 && groups[0].len() == 1 {
        let overhead = geom.pad_per_seg + geom.slice_trail_w;
        let text_budget = limit.saturating_sub(overhead);
        if text_budget == 0 {
            groups.clear();
        } else {
            groups[0][0].text = truncate_to_width(&groups[0][0].text, text_budget, ellipsis);
        }
    }

    groups
}
