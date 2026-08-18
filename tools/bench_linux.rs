#![cfg(target_os = "linux")]

use std::env;
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const O_RDWR: i32 = 0o2;
const O_NOCTTY: i32 = 0o400;
const O_NONBLOCK: i32 = 0o4000;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const TIOCSWINSZ: u64 = 0x5414;
const SC_CLK_TCK: i32 = 2;
const TIMEOUT: Duration = Duration::from_secs(3);
const IDLE_SAMPLE: Duration = Duration::from_millis(500);

#[repr(C)]
struct WinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buffer: *mut i8, len: usize) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn sysconf(name: i32) -> i64;
}

#[derive(Debug)]
struct RunMetrics {
    first_frame_ms: f64,
    input_ready_ms: f64,
    rss_kb: u64,
    pss_kb: u64,
    idle_cpu_percent: f64,
    session_pss_delta_kb: i64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let binary = PathBuf::from(args.next().ok_or("usage: bench_linux <binary> [runs] [json]")?);
    let runs = args
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
        .unwrap_or(10_usize);
    let json_path = args.next().map(PathBuf::from);

    if runs == 0 {
        return Err("runs must be greater than zero".into());
    }
    if !binary.is_file() {
        return Err(format!("binary does not exist: {}", binary.display()).into());
    }

    let binary = fs::canonicalize(binary)?;
    let cli_startup_ms = measure_cli_startup(&binary, 30)?;
    let mut samples = Vec::with_capacity(runs);
    for index in 0..runs {
        let metrics = measure_tui_run(&binary, index)?;
        eprintln!(
            "run {:>2}: frame={:.3}ms input={:.3}ms pss={}KiB rss={}KiB cpu={:.3}% session_delta={}KiB",
            index + 1,
            metrics.first_frame_ms,
            metrics.input_ready_ms,
            metrics.pss_kb,
            metrics.rss_kb,
            metrics.idle_cpu_percent,
            metrics.session_pss_delta_kb
        );
        samples.push(metrics);
    }

    let binary_bytes = fs::metadata(&binary)?.len();
    let report = Report::from_samples(cli_startup_ms, binary_bytes, &samples);
    println!("{}", report.human());

    if let Some(path) = json_path {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, report.json())?;
    }

    Ok(())
}

struct Report {
    runs: usize,
    cli_startup_p50_ms: f64,
    cli_startup_p95_ms: f64,
    first_frame_p50_ms: f64,
    first_frame_p95_ms: f64,
    input_ready_p50_ms: f64,
    input_ready_p95_ms: f64,
    idle_pss_p50_kb: f64,
    idle_rss_p50_kb: f64,
    idle_cpu_p50_percent: f64,
    session_pss_delta_p50_kb: f64,
    binary_bytes: u64,
}

impl Report {
    fn from_samples(cli: Vec<f64>, binary_bytes: u64, samples: &[RunMetrics]) -> Self {
        Self {
            runs: samples.len(),
            cli_startup_p50_ms: percentile_f64(cli.clone(), 0.50),
            cli_startup_p95_ms: percentile_f64(cli, 0.95),
            first_frame_p50_ms: percentile_f64(
                samples.iter().map(|sample| sample.first_frame_ms).collect(),
                0.50,
            ),
            first_frame_p95_ms: percentile_f64(
                samples.iter().map(|sample| sample.first_frame_ms).collect(),
                0.95,
            ),
            input_ready_p50_ms: percentile_f64(
                samples.iter().map(|sample| sample.input_ready_ms).collect(),
                0.50,
            ),
            input_ready_p95_ms: percentile_f64(
                samples.iter().map(|sample| sample.input_ready_ms).collect(),
                0.95,
            ),
            idle_pss_p50_kb: percentile_u64(
                samples.iter().map(|sample| sample.pss_kb).collect(),
                0.50,
            ),
            idle_rss_p50_kb: percentile_u64(
                samples.iter().map(|sample| sample.rss_kb).collect(),
                0.50,
            ),
            idle_cpu_p50_percent: percentile_f64(
                samples
                    .iter()
                    .map(|sample| sample.idle_cpu_percent)
                    .collect(),
                0.50,
            ),
            session_pss_delta_p50_kb: percentile_i64(
                samples
                    .iter()
                    .map(|sample| sample.session_pss_delta_kb)
                    .collect(),
                0.50,
            ),
            binary_bytes,
        }
    }

    fn human(&self) -> String {
        format!(
            concat!(
                "MatrixCode Linux PTY benchmark ({runs} runs)\n",
                "  CLI startup p50/p95: {cli50:.3}/{cli95:.3} ms\n",
                "  First frame p50/p95:  {frame50:.3}/{frame95:.3} ms\n",
                "  Input ready p50/p95:  {input50:.3}/{input95:.3} ms\n",
                "  Idle PSS p50:          {pss:.0} KiB\n",
                "  Idle RSS p50:          {rss:.0} KiB\n",
                "  Idle CPU p50:          {cpu:.3}%\n",
                "  New session PSS delta: {session:.0} KiB\n",
                "  Binary size:           {binary} bytes"
            ),
            runs = self.runs,
            cli50 = self.cli_startup_p50_ms,
            cli95 = self.cli_startup_p95_ms,
            frame50 = self.first_frame_p50_ms,
            frame95 = self.first_frame_p95_ms,
            input50 = self.input_ready_p50_ms,
            input95 = self.input_ready_p95_ms,
            pss = self.idle_pss_p50_kb,
            rss = self.idle_rss_p50_kb,
            cpu = self.idle_cpu_p50_percent,
            session = self.session_pss_delta_p50_kb,
            binary = self.binary_bytes,
        )
    }

    fn json(&self) -> String {
        format!(
            concat!(
                "{{\n",
                "  \"runs\": {runs},\n",
                "  \"cli_startup_p50_ms\": {cli50:.6},\n",
                "  \"cli_startup_p95_ms\": {cli95:.6},\n",
                "  \"first_frame_p50_ms\": {frame50:.6},\n",
                "  \"first_frame_p95_ms\": {frame95:.6},\n",
                "  \"input_ready_p50_ms\": {input50:.6},\n",
                "  \"input_ready_p95_ms\": {input95:.6},\n",
                "  \"idle_pss_p50_kb\": {pss:.3},\n",
                "  \"idle_rss_p50_kb\": {rss:.3},\n",
                "  \"idle_cpu_p50_percent\": {cpu:.6},\n",
                "  \"session_pss_delta_p50_kb\": {session:.3},\n",
                "  \"binary_bytes\": {binary}\n",
                "}}\n"
            ),
            runs = self.runs,
            cli50 = self.cli_startup_p50_ms,
            cli95 = self.cli_startup_p95_ms,
            frame50 = self.first_frame_p50_ms,
            frame95 = self.first_frame_p95_ms,
            input50 = self.input_ready_p50_ms,
            input95 = self.input_ready_p95_ms,
            pss = self.idle_pss_p50_kb,
            rss = self.idle_rss_p50_kb,
            cpu = self.idle_cpu_p50_percent,
            session = self.session_pss_delta_p50_kb,
            binary = self.binary_bytes,
        )
    }
}

fn measure_cli_startup(binary: &Path, runs: usize) -> io::Result<Vec<f64>> {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let status = Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("--version benchmark process failed"));
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(samples)
}

fn measure_tui_run(binary: &Path, index: usize) -> io::Result<RunMetrics> {
    let temp = env::temp_dir().join(format!("matrixcode-bench-{}-{index}", process::id()));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp)?;
    let result = measure_tui_run_inner(binary, &temp);
    let _ = fs::remove_dir_all(&temp);
    result
}

fn measure_tui_run_inner(binary: &Path, temp: &Path) -> io::Result<RunMetrics> {
    let (mut master, slave) = open_pty()?;
    set_nonblocking(&master)?;

    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let stderr = Stdio::from(slave);
    let start = Instant::now();
    let mut child = Command::new(binary)
        .current_dir(temp)
        .env("XDG_DATA_HOME", temp.join("data"))
        .env("HOME", temp)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;

    let first_frame = match read_until(&mut master, b"MatrixCode", TIMEOUT) {
        Ok(()) => start.elapsed().as_secs_f64() * 1000.0,
        Err(error) => return finish_with_error(&mut child, error),
    };

    write_retry(&mut master, "§".as_bytes(), TIMEOUT)?;
    if let Err(error) = read_until(&mut master, "§".as_bytes(), TIMEOUT) {
        return finish_with_error(&mut child, error);
    }
    let input_ready = start.elapsed().as_secs_f64() * 1000.0;

    write_retry(&mut master, &[0x7f], TIMEOUT)?;
    thread::sleep(Duration::from_millis(100));
    let (rss_kb, pss_kb) = memory_kb(child.id())?;
    let idle_cpu_percent = idle_cpu_percent(child.id(), IDLE_SAMPLE)?;

    write_retry(&mut master, b"/new\r", TIMEOUT)?;
    if let Err(error) = read_until(&mut master, b"Started a new session.", TIMEOUT) {
        return finish_with_error(&mut child, error);
    }
    thread::sleep(Duration::from_millis(100));
    let (_, session_pss_kb) = memory_kb(child.id())?;
    let session_pss_delta_kb = session_pss_kb as i64 - pss_kb as i64;

    write_retry(&mut master, &[0x03], TIMEOUT)?;
    wait_or_kill(&mut child, Duration::from_secs(1))?;

    Ok(RunMetrics {
        first_frame_ms: first_frame,
        input_ready_ms: input_ready,
        rss_kb,
        pss_kb,
        idle_cpu_percent,
        session_pss_delta_kb,
    })
}

fn open_pty() -> io::Result<(File, File)> {
    let master_fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
    if master_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master_fd) };

    if unsafe { grantpt(master_fd) } != 0 || unsafe { unlockpt(master_fd) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut name = [0_i8; 256];
    if unsafe { ptsname_r(master_fd, name.as_mut_ptr(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let slave_path = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .map_err(|_| io::Error::other("PTY path is not UTF-8"))?;
    let slave = OpenOptions::new().read(true).write(true).open(slave_path)?;

    let size = WinSize {
        rows: 30,
        cols: 100,
        xpixel: 0,
        ypixel: 0,
    };
    if unsafe { ioctl(master_fd, TIOCSWINSZ, &size) } != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok((master, slave))
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 || unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_until(master: &mut File, marker: &[u8], timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut collected = Vec::with_capacity(4096);
    let mut buffer = [0_u8; 4096];

    while Instant::now() < deadline {
        match master.read(&mut buffer) {
            Ok(0) => thread::sleep(Duration::from_millis(1)),
            Ok(read) => {
                collected.extend_from_slice(&buffer[..read]);
                if contains_bytes(&collected, marker) {
                    return Ok(());
                }
                if collected.len() > 64 * 1024 {
                    let keep = marker.len().saturating_sub(1).max(4096);
                    let start = collected.len().saturating_sub(keep);
                    collected.drain(..start);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.raw_os_error() == Some(5) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out waiting for {:?}", String::from_utf8_lossy(marker)),
    ))
}

fn write_retry(master: &mut File, bytes: &[u8], timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut offset = 0;
    while offset < bytes.len() && Instant::now() < deadline {
        match master.write(&bytes[offset..]) {
            Ok(0) => thread::sleep(Duration::from_millis(1)),
            Ok(written) => offset += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
    if offset == bytes.len() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::TimedOut, "PTY write timed out"))
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|window| window == needle)
}

fn memory_kb(pid: u32) -> io::Result<(u64, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))?;
    let mut rss = None;
    let mut pss = None;
    for line in text.lines() {
        if let Some(value) = parse_kb_line(line, "Rss:") {
            rss = Some(value);
        } else if let Some(value) = parse_kb_line(line, "Pss:") {
            pss = Some(value);
        }
    }
    Ok((
        rss.ok_or_else(|| io::Error::other("Rss missing from smaps_rollup"))?,
        pss.ok_or_else(|| io::Error::other("Pss missing from smaps_rollup"))?,
    ))
}

fn parse_kb_line(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn idle_cpu_percent(pid: u32, duration: Duration) -> io::Result<f64> {
    let ticks_per_second = unsafe { sysconf(SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return Err(io::Error::other("sysconf(_SC_CLK_TCK) failed"));
    }
    let before = process_ticks(pid)?;
    let start = Instant::now();
    thread::sleep(duration);
    let elapsed = start.elapsed().as_secs_f64();
    let after = process_ticks(pid)?;
    let cpu_seconds = (after.saturating_sub(before)) as f64 / ticks_per_second as f64;
    Ok(cpu_seconds / elapsed * 100.0)
}

fn process_ticks(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| io::Error::other("malformed /proc stat"))?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    let user: u64 = fields
        .get(11)
        .ok_or_else(|| io::Error::other("utime missing"))?
        .parse()
        .map_err(|_| io::Error::other("invalid utime"))?;
    let system: u64 = fields
        .get(12)
        .ok_or_else(|| io::Error::other("stime missing"))?
        .parse()
        .map_err(|_| io::Error::other("invalid stime"))?;
    Ok(user + system)
}

fn wait_or_kill(child: &mut Child, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.kill()?;
    let _ = child.wait()?;
    Ok(())
}

fn finish_with_error<T>(child: &mut Child, error: io::Error) -> io::Result<T> {
    let _ = child.kill();
    let _ = child.wait();
    Err(error)
}

fn percentile_f64(mut values: Vec<f64>, percentile: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[index_for(values.len(), percentile)]
}

fn percentile_u64(mut values: Vec<u64>, percentile: f64) -> f64 {
    values.sort_unstable();
    values[index_for(values.len(), percentile)] as f64
}

fn percentile_i64(mut values: Vec<i64>, percentile: f64) -> f64 {
    values.sort_unstable();
    values[index_for(values.len(), percentile)] as f64
}

fn index_for(len: usize, percentile: f64) -> usize {
    (((len.saturating_sub(1)) as f64 * percentile).round() as usize).min(len.saturating_sub(1))
}
