//! 进程自己的内存占用：管理面板右上角那一格用它，5 秒一采样。
//!
//! 读的是**工作集**（任务管理器里「内存」那一列同一个口径）。不引 `windows` / `windows-sys`
//! 依赖——声明方式与本目录 `console.rs` 一致，直接挂 kernel32。
//! 非 Windows 上没实现：插件 ABI 与安装包都只出 Windows，与其留一条本机验不了的分支，
//! 不如让那一格干脆不画（[`working_set_bytes`] 回 `None`）。

use std::time::{Duration, Instant};

/// 面板那一格的采样间隔
pub const SAMPLE_GAP: Duration = Duration::from_secs(5);

/// 工作集字节数；取不到就是 `None`（非 Windows、或系统调用失败）
pub fn working_set_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        win::working_set()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 那一格的文案：够一 GiB 就换单位，否则按 MiB 取整；不足 1 MiB 退回 KiB
pub fn format_working_set(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("内存 {:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("内存 {} MiB", bytes.div_ceil(MIB))
    } else {
        format!("内存 {} KiB", bytes.div_ceil(KIB))
    }
}

/// 采样器：`gap` 之内重复来问都回缓存值，不重复问操作系统
pub struct Sampler {
    gap: Duration,
    last: Option<Instant>,
    value: Option<u64>,
}

impl Sampler {
    pub fn new(gap: Duration) -> Self {
        Sampler {
            gap,
            last: None,
            value: None,
        }
    }

    /// 到点就重采一次并把当前值交回去；没到点就把上次那份原样交回去
    pub fn read(&mut self) -> Option<u64> {
        self.read_at(Instant::now(), working_set_bytes)
    }

    /// 只回缓存值，不触发采样——画面板那一格用
    pub fn peek(&self) -> Option<u64> {
        self.value
    }

    /// 距下一次该问操作系统还有多久（面板靠它决定隔多久重绘一次）
    pub fn next_in(&self) -> Duration {
        match self.last {
            None => Duration::ZERO,
            Some(at) => self.gap.saturating_sub(at.elapsed()),
        }
    }

    /// 时间与取值都从外面进来的那一版：给单测留的缝，这样"5 秒才问一次"能被证伪
    pub fn read_at(&mut self, now: Instant, sample: impl FnOnce() -> Option<u64>) -> Option<u64> {
        if self
            .last
            .is_none_or(|at| now.saturating_duration_since(at) >= self.gap)
        {
            self.value = sample();
            self.last = Some(now);
        }
        self.value
    }
}

#[cfg(windows)]
mod win {
    /// `PROCESS_MEMORY_COUNTERS`（不是 _EX 那一版）：只要工作集，字段按声明顺序排满即可，
    /// `cb` 填这个结构的大小，系统调用自己会校验
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        working_set_size: usize,
        peak_working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    pub fn working_set() -> Option<u64> {
        let mut counters = ProcessMemoryCounters {
            cb: size_of::<ProcessMemoryCounters>() as u32,
            ..Default::default()
        };
        // 不声明 GetCurrentProcess：MSDN 写明它返回的就是伪句柄 (HANDLE)-1，
        // 而 elevate.rs 里那个同名声明返回 isize——同一个符号在本 crate 声明两遍、
        // 签名还不一样，是 rustc 会认、语义不认的那类坑。用常量把这个符号从本模块去掉。
        let this_process = -1isize as *mut core::ffi::c_void;
        // K32 系列返回 BOOL，0 是失败
        let ok = unsafe { K32GetProcessMemoryInfo(this_process, &mut counters, counters.cb) };
        (ok != 0).then_some(counters.working_set_size as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 「5 秒一采样」这条口径得能被证伪：差一毫秒都不该再问操作系统
    #[test]
    fn the_sampler_asks_the_os_once_per_gap() {
        let calls = AtomicUsize::new(0);
        let sample = || {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(300 * 1024 * 1024)
        };
        let base = Instant::now();
        let mut sampler = Sampler::new(SAMPLE_GAP);

        assert_eq!(sampler.read_at(base, sample), Some(300 << 20));
        sampler.read_at(base + Duration::from_millis(4_999), sample);
        sampler.read_at(base + Duration::from_millis(5_000), sample);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "第一下与满 5 秒那下才该问，中间那下不该"
        );
        // 没到点也照样有值可画，且 peek 不会把采样往前推
        assert_eq!(sampler.peek(), Some(300 << 20));
        sampler.read_at(base + Duration::from_millis(5_001), sample);
        assert_eq!(calls.load(Ordering::Relaxed), 2, "1 毫秒的余量不算到点");
    }

    /// 刚建好还没采过时，重绘该是"现在就问"，不是等满 5 秒
    #[test]
    fn a_fresh_sampler_wants_an_immediate_sample() {
        let mut sampler = Sampler::new(SAMPLE_GAP);
        assert_eq!(sampler.next_in(), Duration::ZERO);
        assert_eq!(sampler.peek(), None);
        sampler.read_at(Instant::now(), || Some(1));
        assert!(sampler.next_in() > Duration::ZERO, "采过之后才该往后等");
    }

    #[test]
    fn the_label_switches_units() {
        assert_eq!(format_working_set(512 * 1024), "内存 512 KiB");
        assert_eq!(format_working_set(312 * 1024 * 1024), "内存 312 MiB");
        assert_eq!(format_working_set(1536 * 1024 * 1024), "内存 1.5 GiB");
        // 恰好跨档不该跳单位：1023 MiB 还是 MiB
        assert_eq!(format_working_set(1023 * 1024 * 1024), "内存 1023 MiB");
    }

    /// 本机就是 Windows，这条读的是真系统调用：取不到值说明 FFI 声明错了
    #[cfg(windows)]
    #[test]
    fn the_real_call_returns_a_plausible_working_set() {
        let bytes = working_set_bytes().expect("Windows 上该读得到自己的工作集");
        assert!(bytes > 2 << 20, "工作集小得可疑: {bytes}");
        assert!(bytes < 64 << 30, "工作集大得可疑: {bytes}");
    }
}
