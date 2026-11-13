//! Percentiles of a series, the slope of a time series, and the flat `name=value` result a run writes.

/// A sorted series of finite numbers.
pub struct Series(Vec<f64>);

impl Series {
    pub fn of(values: impl Iterator<Item = f64>) -> Series {
        let mut v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
        v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        Series(v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The value at fraction `p` of the sorted series (nearest rank); NaN when it is empty.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        self.0[((self.0.len() - 1) as f64 * p).round() as usize]
    }

    pub fn median(&self) -> f64 {
        self.percentile(0.5)
    }

    pub fn min(&self) -> f64 {
        self.0.first().copied().unwrap_or(f64::NAN)
    }

    pub fn max(&self) -> f64 {
        self.0.last().copied().unwrap_or(f64::NAN)
    }
}

/// The least-squares slope of `(x, y)` points, in y per x; 0 with fewer than two points or no spread in x.
pub fn slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if points.len() < 2 {
        return 0.0;
    }
    let (mx, my) = (points.iter().map(|p| p.0).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let var: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if var == 0.0 {
        return 0.0;
    }
    points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / var
}

/// The result of a run: `name=value` lines in the order they were added.
#[derive(Default)]
pub struct Fields(Vec<(String, String)>);

impl Fields {
    pub fn new() -> Fields {
        Fields::default()
    }

    pub fn text(&mut self, name: &str, value: impl Into<String>) {
        let value: String = value.into();
        let one_line: String = value.chars().map(|c| if c == '\r' || c == '\n' { ' ' } else { c }).collect();
        self.0.push((name.to_string(), one_line));
    }

    pub fn num(&mut self, name: &str, value: f64) {
        self.text(name, if value.is_finite() { format!("{value:.3}") } else { "nan".to_string() });
    }

    /// `<name>_median`, `_p95`, `_p99`, `_max`, `_min` and `_n` of a series.
    pub fn series(&mut self, name: &str, s: &Series) {
        self.num(&format!("{name}_median"), s.median());
        self.num(&format!("{name}_p95"), s.percentile(0.95));
        self.num(&format!("{name}_p99"), s.percentile(0.99));
        self.num(&format!("{name}_max"), s.max());
        self.num(&format!("{name}_min"), s.min());
        self.num(&format!("{name}_n"), s.len() as f64);
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn render(&self) -> String {
        self.0.iter().map(|(n, v)| format!("{n}={v}\n")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank_and_an_empty_series_is_nan() {
        let s = Series::of((1..=100).map(f64::from));
        assert_eq!(
            (s.median(), s.percentile(0.95), s.percentile(0.99), s.min(), s.max()),
            (51.0, 95.0, 99.0, 1.0, 100.0)
        );
        let none = Series::of(std::iter::empty());
        assert!(none.median().is_nan() && none.max().is_nan());
        // A value that is not finite never enters a series.
        assert_eq!(Series::of([1.0, f64::NAN, 3.0].into_iter()).len(), 2);
    }

    #[test]
    fn the_slope_of_a_line_is_its_rise_per_step() {
        let line: Vec<(f64, f64)> = (0..10).map(|x| (f64::from(x) * 2.0, 100.0 + f64::from(x))).collect();
        assert!((slope(&line) - 0.5).abs() < 1e-9);
        assert_eq!(slope(&[(1.0, 5.0)]), 0.0);
        assert_eq!(slope(&[(1.0, 5.0), (1.0, 9.0)]), 0.0);
    }

    #[test]
    fn fields_render_one_line_each_in_order() {
        let mut f = Fields::new();
        f.text("config", "1920x1080@60\nnvenc");
        f.num("b1", 12.34567);
        f.num("none", f64::NAN);
        assert_eq!(f.render(), "config=1920x1080@60 nvenc\nb1=12.346\nnone=nan\n");
        assert_eq!(f.get("b1"), Some("12.346"));
    }
}
