//! パーセンタイルなどの統計（純粋な関数）。

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct Percentiles {
    pub count: usize,
    pub min: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
    pub mean: f64,
}

/// 最近傍順位法（nearest-rank）: n 個を昇順に並べたとき、p% 以上を含む最小の順位の値。
/// `permille` は千分率（99.9% なら 999）。浮動小数点の誤差で順位がずれないよう、整数で計算する。
fn nearest_rank(sorted: &[u64], permille: usize) -> u64 {
    let n = sorted.len();
    let rank = (n * permille).div_ceil(1000);
    sorted[rank.clamp(1, n) - 1]
}

/// 値の列（順不同、破壊的に並べ替える）から統計を作る。空なら `None`。
pub fn percentiles(values: &mut [u64]) -> Option<Percentiles> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let sum: u128 = values.iter().map(|v| u128::from(*v)).sum();
    Some(Percentiles {
        count: values.len(),
        min: values[0],
        p50: nearest_rank(values, 500),
        p90: nearest_rank(values, 900),
        p99: nearest_rank(values, 990),
        p999: nearest_rank(values, 999),
        max: values[values.len() - 1],
        mean: sum as f64 / values.len() as f64,
    })
}

impl Percentiles {
    pub fn to_json(&self) -> Value {
        json!({
            "count": self.count, "min": self.min, "p50": self.p50, "p90": self.p90,
            "p99": self.p99, "p999": self.p999, "max": self.max, "mean": self.mean,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_percentiles() {
        assert_eq!(percentiles(&mut []), None);
    }

    #[test]
    fn nearest_rank_on_1_to_100() {
        let mut v: Vec<u64> = (1..=100).rev().collect();
        let p = percentiles(&mut v).expect("some");
        assert_eq!(
            (p.min, p.p50, p.p90, p.p99, p.p999, p.max),
            (1, 50, 90, 99, 100, 100)
        );
        assert_eq!(p.count, 100);
        assert!((p.mean - 50.5).abs() < 1e-9);
    }

    #[test]
    fn single_value_and_ties() {
        let p = percentiles(&mut [7]).expect("some");
        assert_eq!((p.p50, p.p99, p.max), (7, 7, 7));
        let p = percentiles(&mut [5, 5, 5, 5, 100]).expect("some");
        assert_eq!((p.p50, p.p90, p.max), (5, 100, 100));
    }

    #[test]
    fn p99_of_ten_thousand_points_is_the_9900th() {
        let mut v: Vec<u64> = (1..=10_000).collect();
        let p = percentiles(&mut v).expect("some");
        assert_eq!((p.p99, p.p999), (9_900, 9_990));
    }
}
