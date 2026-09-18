pub fn human_size(n: u64) -> String {
    let mut n = n as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 {
            return if unit == "B" {
                format!("{:.0}{}", n, unit)
            } else {
                format!("{:.1}{}", n, unit)
            };
        }
        n /= 1024.0;
    }
    format!("{:.1}PB", n)
}
