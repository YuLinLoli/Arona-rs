//! 进程自己的内存占用：管理面板右上角那一格用它，5 秒一采样。
//!
//! 口径是**专用工作集**（private working set）——任务管理器「进程」页那一列显示的就是它。
//! 一开始这里读的是 `PROCESS_MEMORY_COUNTERS::WorkingSetSize`（完整工作集，含共享页），
//! 在部署机上比任务管理器多出一倍半（面板 331 MiB / 任务管理器 115.4 MB），被当成"内存涨了 3 倍"；
//! 共享页里大头是显卡驱动与 DLL 映像，换机器比例就变，所以那一档不能当"内存占用"报给用户。
//! 现在改成：`VirtualQuery` 走一遍地址空间收集已提交（`MEM_COMMIT`）的页，再用
//! `K32QueryWorkingSetEx` 逐页判 `ShareCount == 0` 求和——与任务管理器同一口径，
//! 面板上的数才对得上。完整工作集那条调用只留给单测当「专用 ≤ 完整」的不变式 oracle。
//!
//! 不引 `windows` / `windows-sys` 依赖——声明方式与本目录 `console.rs` 一致，直接挂 kernel32。
//! 非 Windows 上没实现：插件 ABI 与安装包都只出 Windows，与其留一条本机验不了的分支，
//! 不如让那一格干脆不画（[`process_memory_bytes`] 回 `None`）。

use std::time::{Duration, Instant};

/// 面板那一格的采样间隔
pub const SAMPLE_GAP: Duration = Duration::from_secs(5);

/// 一页的元数据里我们只关心两件事：这页有效吗、它被几个进程共享
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageInfo {
    pub valid: bool,
    pub share_count: u8,
}

/// 专用工作集 = 有效且未被共享（`ShareCount == 0`）的那些页之和
pub fn sum_private_bytes(pages: &[PageInfo], page_size: u64) -> u64 {
    pages
        .iter()
        .filter(|page| page.valid && page.share_count == 0)
        .count() as u64
        * page_size
}

/// 进程专用工作集字节数；取不到就是 `None`（非 Windows、或系统调用失败）
pub fn process_memory_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        win::private_working_set()
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
        self.read_at(Instant::now(), process_memory_bytes)
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
    use super::{PageInfo, sum_private_bytes};

    /// `PSAPI_WORKING_SET_EX_INFORMATION`：一个虚拟地址 + 一个 32 位的属性位域
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct WorkingSetExInformation {
        pub(super) virtual_address: usize,
        pub(super) virtual_attributes: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32QueryWorkingSetEx(
            process: *mut core::ffi::c_void,
            buffer: *mut WorkingSetExInformation,
            cb: u32,
        ) -> i32;
        fn VirtualQuery(
            address: *const core::ffi::c_void,
            buffer: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
        fn GetSystemInfo(system_info: *mut core::ffi::c_void);
    }

    /// `MEMORY_BASIC_INFORMATION`（`VirtualQuery` 的输出）
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct MemoryBasicInformation {
        base_address: *mut core::ffi::c_void,
        allocation_base: *mut core::ffi::c_void,
        allocation_protect: u32,
        region_size: usize,
        state: u32,
        protect: u32,
        type_: u32,
    }

    const MEM_COMMIT: u32 = 0x0000_1000;

    // 不声明 GetCurrentProcess：MSDN 写明它返回的就是伪句柄 (HANDLE)-1，
    // 而 elevate.rs 里那个同名声明返回 isize——同一个符号在本 crate 声明两遍、
    // 签名还不一样，是 rustc 会认、语义不认的那类坑。用常量把这个符号从本模块去掉。
    fn this_process() -> *mut core::ffi::c_void {
        -1isize as *mut core::ffi::c_void
    }

    /// `SYSTEM_INFO` 只用到偏移 4 上的 `dwPageSize`（前面那个 union 是 4 字节）。
    /// 不去复刻整个结构（里面有 union 与平台相关的尾部），给足缓冲区按小端读那四个字节。
    fn page_size() -> Option<usize> {
        let mut info = [0u8; 128];
        unsafe { GetSystemInfo(info.as_mut_ptr().cast()) };
        let size = u32::from_le_bytes([info[4], info[5], info[6], info[7]]) as usize;
        (size >= 512).then_some(size)
    }

    /// 走一遍本进程的虚拟地址空间，收所有 `MEM_COMMIT` 区段的起点与页数。
    /// `K32QueryWorkingSetEx` 是**按地址逐页问**的：数组里给的是"要查哪些页"，
    /// 传一片零地址进去就只会反复查零地址那一页（第一版就栽在这里，读回来恒为 0）。
    fn committed_regions(page: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut address = 0x10000usize; // 低 64 KB 是保留区，从这往上走
        let limit = 0x0000_7fff_ffff_ffffusize; // 用户态上界
        let mut info = MemoryBasicInformation::default();
        while address < limit {
            let got = unsafe {
                VirtualQuery(
                    address as *const core::ffi::c_void,
                    &mut info,
                    size_of::<MemoryBasicInformation>(),
                )
            };
            if got == 0 || info.region_size == 0 {
                break;
            }
            if info.state == MEM_COMMIT {
                out.push((info.base_address as usize, info.region_size / page));
            }
            address += info.region_size;
        }
        out
    }

    /// 逐页问工作集属性，解成 `PageInfo` 列表
    fn query_pages(page: usize) -> Option<Vec<PageInfo>> {
        let regions = committed_regions(page);
        let total: usize = regions.iter().map(|(_, pages)| *pages).sum();
        let mut entries: Vec<WorkingSetExInformation> = Vec::with_capacity(total);
        for (base, pages) in regions {
            entries.extend((0..pages).map(|offset| WorkingSetExInformation {
                virtual_address: base + offset * page,
                virtual_attributes: 0,
            }));
        }
        if !entries.is_empty() {
            let cb = (entries.len() * size_of::<WorkingSetExInformation>()) as u32;
            let ok = unsafe { K32QueryWorkingSetEx(this_process(), entries.as_mut_ptr(), cb) };
            if ok == 0 {
                return None;
            }
        }
        Some(entries.iter().map(decode).collect())
    }

    /// `PSAPI_WORKING_SET_EX_BLOCK`：bit0 是 Valid，bit1..3 是 ShareCount
    pub(super) fn decode(entry: &WorkingSetExInformation) -> PageInfo {
        PageInfo {
            valid: entry.virtual_attributes & 1 == 1,
            share_count: ((entry.virtual_attributes >> 1) & 0b111) as u8,
        }
    }

    pub fn private_working_set() -> Option<u64> {
        let page = page_size()?;
        let pages = query_pages(page)?;
        Some(sum_private_bytes(&pages, page as u64))
    }

    /// 完整工作集字节数：只给单测卡「专用 ≤ 完整」这条不变式用。
    /// 生产路径已经不读它了——第一版拿它当"内存占用"报给用户，口径和任务管理器差一倍半。
    #[cfg(test)]
    pub(super) fn full_working_set() -> Option<u64> {
        full_set::working_set_size().map(|size| size as u64)
    }

    #[cfg(test)]
    mod full_set {
        use super::this_process;

        /// `PROCESS_MEMORY_COUNTERS`（不是 _EX 那一版）：`cb` 填结构大小，系统调用自己会校验
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

        pub(super) fn working_set_size() -> Option<usize> {
            let mut counters = ProcessMemoryCounters {
                cb: size_of::<ProcessMemoryCounters>() as u32,
                ..Default::default()
            };
            let ok = unsafe { K32GetProcessMemoryInfo(this_process(), &mut counters, counters.cb) };
            (ok != 0).then_some(counters.working_set_size)
        }
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

    /// 专用工作集只数"有效且 ShareCount==0"的页：共享页（DLL 映像、驱动映射）一律不算
    #[test]
    fn shared_and_invalid_pages_are_not_counted_as_private() {
        let page = 4096u64;
        let pages = [
            PageInfo {
                valid: true,
                share_count: 0,
            },
            PageInfo {
                valid: true,
                share_count: 0,
            },
            PageInfo {
                valid: true,
                share_count: 1,
            },
            PageInfo {
                valid: true,
                share_count: 7,
            },
            PageInfo {
                valid: false,
                share_count: 0,
            },
        ];
        assert_eq!(sum_private_bytes(&pages, page), 2 * page);
        assert_eq!(sum_private_bytes(&[], page), 0);
    }

    /// 位域读法钉住：bit0=Valid，bit1..3=ShareCount。读错一位，面板那个数就整体偏
    #[cfg(windows)]
    #[test]
    fn the_working_set_flags_decode_as_windows_defines_them() {
        use super::win::WorkingSetExInformation;
        use super::win::decode;
        let entry = WorkingSetExInformation {
            virtual_address: 0,
            virtual_attributes: 0b101,
        };
        let info = decode(&entry);
        assert!(
            info.valid && info.share_count == 2,
            "bit0 是有效位，bit1..3 才是共享数"
        );
        let shared = decode(&WorkingSetExInformation {
            virtual_address: 0,
            virtual_attributes: 0b1000,
        });
        assert!(!shared.valid && shared.share_count == 4);
    }

    /// 本机就是 Windows，这条读的是真系统调用：取不到值或读回 0 说明 FFI 声明、
    /// 地址填法错了；「专用 ≤ 完整」是这条口径的硬不变式（第一版传零地址进去读回 0，就靠它拦住）。
    /// 这里**不卡绝对下限**：那个数多大取决于这个进程此刻摸过多少私有页，
    /// 单独跑这一条时整个进程才 0.8 MiB（其余全是共享的 DLL 映像页），卡下限是在测进程不是在测读数。
    #[cfg(windows)]
    #[test]
    fn the_real_call_reads_a_private_working_set_inside_the_full_one() {
        let bytes = process_memory_bytes().expect("Windows 上该读得到自己的专用工作集");
        assert!(bytes > 0, "专用工作集读回 0：逐页问那一路没拿到有效页");
        let full = super::win::full_working_set().expect("完整工作集该读得到");
        assert!(
            bytes <= full,
            "专用工作集 {bytes} 不可能大于完整工作集 {full}"
        );
        println!(
            "PID {} 专用 {:.1} MiB 完整 {:.1} MiB",
            std::process::id(),
            bytes as f64 / (1 << 20) as f64,
            full as f64 / (1 << 20) as f64
        );
    }

    /// 真私有 32 MiB 就必须涨 32 MiB：这条才钉得住"逐页问到的确实是自己的私有页"，
    /// 而且与进程本来多大无关。只 `vec!` 不写不算——提交页要第一次落地才进工作集，
    /// 所以按 4096 的步长一页碰一下（步长取页大小，缓冲区起点对不对齐都保证每下换一个页）。
    #[cfg(windows)]
    #[test]
    fn pages_actually_touched_show_up_in_the_reading() {
        const MIB: u64 = 1 << 20;
        const PAGE: usize = 4096;
        let before = process_memory_bytes().expect("摸页之前该读得到");
        let mut buffer = vec![0u8; 32 * MIB as usize];
        for page in buffer.chunks_mut(PAGE) {
            page[0] = 0xA5;
        }
        let after = process_memory_bytes().expect("摸页之后该读得到");
        std::hint::black_box(&buffer);
        let grew = after.saturating_sub(before);
        assert!(
            grew >= 30 * MIB,
            "摸了 32 MiB 私有页，读数只涨了 {} KiB",
            grew / 1024
        );
        println!(
            "摸页前 {:.1} MiB → 摸页后 {:.1} MiB",
            before as f64 / MIB as f64,
            after as f64 / MIB as f64
        );
    }
}
