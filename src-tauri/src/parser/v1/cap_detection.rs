pub const UNAVAILABLE_DAMAGE_CAP: i32 = 99_999_999;

pub fn is_cap_known(cap: Option<i32>) -> bool {
    matches!(cap, Some(cap) if cap > 0 && cap < UNAVAILABLE_DAMAGE_CAP)
}

pub fn learn_crit_multipliers(at_or_over_cap: impl Iterator<Item = (i32, i32)>) -> Vec<f64> {
    use std::collections::HashMap;

    const BUCKET: f64 = 0.002;
    let mut counts: HashMap<i64, u64> = HashMap::new();
    let mut total: u64 = 0;

    for (damage, cap) in at_or_over_cap {
        if !is_cap_known(Some(cap)) || damage < cap {
            continue;
        }

        let ratio = damage as f64 / cap as f64;
        *counts.entry((ratio / BUCKET).round() as i64).or_default() += 1;
        total += 1;
    }

    if total == 0 {
        return Vec::new();
    }

    let threshold = (total / 100).max(3);
    counts
        .into_iter()
        .filter(|(_, count)| *count >= threshold)
        .map(|(bucket, _)| bucket as f64 * BUCKET)
        .collect()
}

pub fn is_capped(damage: i32, cap: Option<i32>, crit_multipliers: &[f64]) -> bool {
    let Some(cap) = cap else { return false };
    if !is_cap_known(Some(cap)) || damage < cap {
        return false;
    }

    if crit_multipliers.is_empty() {
        return true;
    }

    let tolerance = (0.003 * damage as f64).max(2.0);
    crit_multipliers
        .iter()
        .any(|&multiplier| (cap as f64 * multiplier - damage as f64).abs() <= tolerance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_recurring_crit_multipliers() {
        let mut data = Vec::new();
        for _ in 0..50 {
            data.push((1000, 1000));
            data.push((1200, 1000));
            data.push((1320, 1000));
        }
        data.push((1037, 1000));
        data.push((1041, 1000));

        let multipliers = learn_crit_multipliers(data.into_iter());
        assert!(multipliers.iter().any(|value| (value - 1.0).abs() < 0.01));
        assert!(multipliers.iter().any(|value| (value - 1.2).abs() < 0.01));
        assert!(multipliers.iter().any(|value| (value - 1.32).abs() < 0.01));
        assert!(!multipliers
            .iter()
            .any(|value| (value - 1.037).abs() < 0.005));
    }

    #[test]
    fn detects_caps_at_crit_peaks() {
        let multipliers = vec![1.0, 1.2, 1.32];
        assert!(is_capped(1000, Some(1000), &multipliers));
        assert!(is_capped(1200, Some(1000), &multipliers));
        assert!(is_capped(1319, Some(1000), &multipliers));
        assert!(!is_capped(1080, Some(1000), &multipliers));
    }

    #[test]
    fn handles_missing_invalid_and_sparse_cap_data() {
        assert!(!is_capped(500, Some(1000), &[1.0, 1.2]));
        assert!(!is_capped(9999, Some(-1), &[1.0, 1.2]));
        assert!(!is_capped(9999, Some(0), &[1.0, 1.2]));
        assert!(!is_capped(9999, None, &[1.0, 1.2]));
        assert!(!is_capped(
            UNAVAILABLE_DAMAGE_CAP,
            Some(UNAVAILABLE_DAMAGE_CAP),
            &[]
        ));
        assert!(is_capped(1000, Some(1000), &[]));
    }
}
