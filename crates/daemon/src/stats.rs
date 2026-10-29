//! Сэмплер RSS/CPU процесса демона для карт «Диагностики» (Stats, 0.3).
//!
//! CPU — дельта между вызовами: суммарное CPU-время процесса делится на
//! прошедшее стеночное время; первый вызов отдаёт 0.0 (базовая точка).
//! Источники: unix — `ps -o rss=,time= -p PID`, windows — `Get-Process`
//! через [`crate::ps_query`]. Сбой опроса — `(0, 0.0)`: карты
//! диагностики показывают «—», Stats не падает.

use std::sync::Mutex;
use std::time::Instant;

/// Сэмплер с памятью предыдущего замера (дельта CPU).
pub struct ProcessSampler {
    /// (момент, суммарное CPU-время в секундах) предыдущего замера.
    last: Mutex<Option<(Instant, f64)>>,
}

impl ProcessSampler {
    /// Пустой сэмплер: первый замер — базовая точка (cpu 0.0).
    pub fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    /// Замер: (RSS в байтах, CPU % между этим и предыдущим вызовом).
    pub async fn sample(&self, pid: u32) -> (u64, f32) {
        let Some((rss_kb, cpu_secs)) = query_process(pid).await else {
            return (0, 0.0);
        };
        let now = Instant::now();
        let mut last = self.last.lock().expect("сэмплер не отравлен");
        let cpu_pct = match *last {
            Some((prev_at, prev_cpu)) if cpu_secs >= prev_cpu => {
                let wall = now.duration_since(prev_at).as_secs_f64();
                if wall > 0.0 {
                    ((cpu_secs - prev_cpu) / wall * 100.0) as f32
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        *last = Some((now, cpu_secs));
        (rss_kb * 1024, cpu_pct)
    }
}

/// (RSS в килобайтах, суммарное CPU-время в секундах) процесса.
#[cfg(unix)]
async fn query_process(pid: u32) -> Option<(u64, f64)> {
    let out = tokio::process::Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps_line(&String::from_utf8_lossy(&out.stdout))
}

/// (RSS в килобайтах, суммарное CPU-время в секундах) процесса.
/// `Get-Process.CPU` — секунды; у только что стартовавшего процесса
/// может быть пустым (трактуется как 0).
#[cfg(windows)]
async fn query_process(pid: u32) -> Option<(u64, f64)> {
    let text = crate::ps_query::run_ps_readonly(&[format!(
        "$p = Get-Process -Id {pid}; Write-Output ($p.WorkingSet64.ToString() + ' ' + $p.CPU)"
    )])
    .await
    .ok()?;
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let mut parts = line.split_whitespace();
    let rss_bytes: u64 = parts.next()?.parse().ok()?;
    let cpu: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    Some((rss_bytes / 1024, cpu))
}

/// Разбор строки `ps -o rss=,time=`: « 12345 00:01:23».
#[cfg(unix)]
fn parse_ps_line(text: &str) -> Option<(u64, f64)> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let mut parts = line.split_whitespace();
    let rss: u64 = parts.next()?.parse().ok()?;
    let cpu = parse_ps_time(parts.next()?)?;
    Some((rss, cpu))
}

/// CPU-время `ps`: `mm:ss`, `hh:mm:ss` или `dd-hh:mm:ss` → секунды.
#[cfg(unix)]
fn parse_ps_time(text: &str) -> Option<f64> {
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, text),
    };
    let fields: Vec<&str> = rest.split(':').collect();
    let seconds: f64 = match fields.as_slice() {
        [m, s] => m.parse::<f64>().ok()? * 60.0 + s.parse::<f64>().ok()?,
        [h, m, s] => {
            h.parse::<f64>().ok()? * 3600.0
                + m.parse::<f64>().ok()? * 60.0
                + s.parse::<f64>().ok()?
        }
        _ => return None,
    };
    Some(days * 86400.0 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ps_line_parses_rss_and_time() {
        assert_eq!(parse_ps_line("  12345 00:01:23\n"), Some((12345, 83.0)));
        assert_eq!(parse_ps_line("999 01:00:00"), Some((999, 3600.0)));
        assert_eq!(
            parse_ps_line("1 2-03:04:05"),
            Some((1, 2.0 * 86400.0 + 3.0 * 3600.0 + 4.0 * 60.0 + 5.0))
        );
        assert_eq!(parse_ps_line("12 00:42"), Some((12, 42.0)));
        assert_eq!(parse_ps_line("мусор"), None);
        assert_eq!(parse_ps_line(""), None);
    }

    #[tokio::test]
    async fn sampler_reports_own_process_and_cpu_delta() {
        let sampler = ProcessSampler::new();
        let pid = std::process::id();
        let (rss1, cpu1) = sampler.sample(pid).await;
        assert!(rss1 > 0, "свой RSS известен");
        assert_eq!(cpu1, 0.0, "первый замер — базовая точка");

        // Нагрузить CPU, затем второй замер: дельта положительна.
        // `ps time` отдаёт ЦЕЛЫЕ секунды: короткая нагрузка может не
        // переступить границу секунды (флаки на CI), поэтому крутимся,
        // пока счётчик CPU реально не сдвинется (дедлайн 10 с).
        let baseline = query_process(pid).await.expect("ps доступен").1;
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut advanced = false;
        let mut x = 0u64;
        while Instant::now() < deadline {
            let burst = Instant::now() + std::time::Duration::from_millis(100);
            while Instant::now() < burst {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            }
            if let Some((_, cpu)) = query_process(pid).await {
                if cpu > baseline {
                    advanced = true;
                    break;
                }
            }
        }
        std::hint::black_box(x);
        assert!(advanced, "счётчик CPU ps сдвинулся под нагрузкой");
        let (_rss2, cpu2) = sampler.sample(pid).await;
        assert!(cpu2 > 0.0, "после нагрузки дельта CPU положительна: {cpu2}");
    }

    #[tokio::test]
    async fn dead_pid_samples_as_zeros() {
        let sampler = ProcessSampler::new();
        // PID вряд ли существует (и не наш точно).
        assert_eq!(sampler.sample(4_000_000).await, (0, 0.0));
    }
}
