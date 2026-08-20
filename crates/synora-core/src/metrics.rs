//! Hand-rolled Prometheus text-format registry (spec §36–§38).
//! The spec fixes metric names, not a library; ~150 lines beats a dependency.
//! Upgrade to the `prometheus` crate if high-cardinality needs arise.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricType {
    Gauge,
    Counter,
}

impl MetricType {
    fn as_str(&self) -> &'static str {
        match self {
            MetricType::Gauge => "gauge",
            MetricType::Counter => "counter",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    name: String,
    /// Label pairs, kept sorted for a stable identity.
    labels: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct Entry {
    kind: MetricType,
    value: f64,
}

/// Thread-safe minimal metrics registry.
pub struct Metrics {
    entries: std::sync::Mutex<HashMap<Key, Entry>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Metrics {
            entries: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn write(&self, kind: MetricType, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut sorted: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        sorted.sort();
        let key = Key {
            name: name.to_string(),
            labels: sorted,
        };
        let mut entries = self.entries.lock().unwrap();
        entries.insert(key, Entry { kind, value });
    }

    pub fn set_gauge(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.write(MetricType::Gauge, name, labels, value);
    }

    /// Set a per-job gauge and drop leftover samples of the same metric
    /// that still carry this job label (usually a previous worker).
    /// Grafana `max by (exported_job)` would otherwise pick Failed over
    /// Running when both `worker=local` and `worker=mirror-zfs` exist.
    pub fn set_job_gauge(&self, name: &str, job: &str, labels: &[(&str, &str)], value: f64) {
        let mut sorted: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        sorted.sort();
        let key = Key {
            name: name.to_string(),
            labels: sorted,
        };
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|existing, _| {
            !(existing.name == name && existing.labels.iter().any(|(n, v)| n == "job" && v == job))
        });
        entries.insert(
            key,
            Entry {
                kind: MetricType::Gauge,
                value,
            },
        );
    }

    pub fn inc_counter(&self, name: &str, labels: &[(&str, &str)], delta: f64) {
        let mut sorted: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        sorted.sort();
        let key = Key {
            name: name.to_string(),
            labels: sorted,
        };
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.entry(key).or_insert(Entry {
            kind: MetricType::Counter,
            value: 0.0,
        });
        entry.value += delta;
    }

    /// Drop every sample labeled with this job so a deleted job leaves no
    /// gauge/counter residue in `/metrics`.
    pub fn remove_job(&self, job: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|key, _| !key.labels.iter().any(|(n, v)| n == "job" && v == job));
    }

    /// Prometheus exposition format (one TYPE line per metric, then samples).
    pub fn render(&self) -> String {
        let entries = self.entries.lock().unwrap();
        let mut samples: Vec<(String, MetricType, String)> = Vec::new();
        for (key, entry) in entries.iter() {
            let mut s = key.name.clone();
            if !key.labels.is_empty() {
                let l = key
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
                    .collect::<Vec<_>>()
                    .join(",");
                s.push_str(&format!("{{{l}}}"));
            }
            s.push_str(&format!(" {}", entry.value));
            samples.push((key.name.clone(), entry.kind, s));
        }
        samples.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        let mut out = String::new();
        let mut last: Option<&str> = None;
        for (name, kind, sample) in &samples {
            if last != Some(name.as_str()) {
                out.push_str("# TYPE ");
                out.push_str(name);
                out.push(' ');
                out.push_str(kind.as_str());
                out.push('\n');
                last = Some(name.as_str());
            }
            out.push_str(sample);
            out.push('\n');
        }
        out
    }
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_prometheus_text() {
        let m = Metrics::new();
        m.set_gauge(
            "synora_job_status",
            &[("job", "ubuntu"), ("worker", "local")],
            1.0,
        );
        m.inc_counter("synora_job_runs_total", &[("job", "ubuntu")], 2.0);
        let out = m.render();
        assert!(out.contains("# TYPE synora_job_status gauge"), "{out}");
        assert!(
            out.contains("synora_job_status{job=\"ubuntu\",worker=\"local\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("# TYPE synora_job_runs_total counter"),
            "{out}"
        );
        assert!(
            out.contains("synora_job_runs_total{job=\"ubuntu\"} 2"),
            "{out}"
        );
    }

    #[test]
    fn labels_sorted_for_identity() {
        let m = Metrics::new();
        m.set_gauge("x", &[("a", "1"), ("b", "2")], 1.0);
        m.set_gauge("x", &[("b", "2"), ("a", "1")], 3.0); // same identity, overwrites
        let out = m.render();
        assert_eq!(out.matches("x{").count(), 1); // one sample (TYPE lines have no braces)
        assert!(out.contains("x{a=\"1\",b=\"2\"} 3"), "{out}");
    }

    #[test]
    fn label_escaping() {
        let m = Metrics::new();
        m.set_gauge("x", &[("k", "a\"b\\c")], 1.0);
        let out = m.render();
        assert!(out.contains("x{k=\"a\\\"b\\\\c\"} 1"), "{out}");
    }

    #[test]
    fn remove_job_drops_labeled_samples() {
        let m = Metrics::new();
        m.set_gauge(
            "synora_job_status",
            &[("job", "gone"), ("worker", "w")],
            3.0,
        );
        m.set_gauge(
            "synora_job_status",
            &[("job", "keep"), ("worker", "w")],
            5.0,
        );
        m.inc_counter("synora_job_runs_total", &[("job", "gone")], 1.0);
        m.remove_job("gone");
        let out = m.render();
        assert!(!out.contains("gone"), "{out}");
        assert!(out.contains("job=\"keep\""), "{out}");
    }

    #[test]
    fn set_job_gauge_replaces_stale_worker_series() {
        let m = Metrics::new();
        m.set_job_gauge(
            "synora_job_status",
            "rustup",
            &[("job", "rustup"), ("worker", "local")],
            6.0,
        );
        m.set_job_gauge(
            "synora_job_status",
            "rustup",
            &[("job", "rustup"), ("worker", "mirror-zfs")],
            4.0,
        );
        let out = m.render();
        assert!(!out.contains("worker=\"local\""), "{out}");
        assert!(out.contains("worker=\"mirror-zfs\""), "{out}");
        assert_eq!(out.matches("synora_job_status{").count(), 1, "{out}");
        assert!(out.contains("} 4"), "{out}");
    }
}
