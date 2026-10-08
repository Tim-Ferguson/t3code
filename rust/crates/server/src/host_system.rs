//! Native OS readings matching the counters Node/libuv exposes to HostResources.
//! No background polling, shell parsing of CPU statistics, or provider launches.
//! Counter mappings: Node v24.13.1 deps/uv/src/{unix/darwin.c,unix/linux.c,win/util.c}.
//! Windows deliberately uses GetSystemInfo like that libuv version; processor-group
//! coverage beyond its reported CPU count is not fabricated by this adapter.
#![cfg_attr(target_os = "macos", allow(deprecated))]

#[cfg(target_os = "macos")]
struct HostPort(libc::mach_port_t);
#[cfg(target_os = "macos")]
impl HostPort {
    fn new() -> Self {
        Self(unsafe { libc::mach_host_self() })
    }
}
#[cfg(target_os = "macos")]
impl Drop for HostPort {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn mach_port_deallocate(
                task: libc::mach_port_t,
                name: libc::mach_port_t,
            ) -> libc::kern_return_t;
        }
        // mach_host_self returns an acquired send right; mach_task_self is borrowed.
        unsafe {
            mach_port_deallocate(libc::mach_task_self(), self.0);
        }
    }
}
use crate::host_resources::{Cpu, Memory};

#[cfg(target_os = "macos")]
pub fn cpu() -> Cpu {
    unsafe {
        let host = HostPort::new();
        let mut count = 0;
        let mut info: libc::processor_info_array_t = std::ptr::null_mut();
        let mut length = 0;
        let result = libc::host_processor_info(
            host.0,
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut count,
            &mut info,
            &mut length,
        );
        if result != libc::KERN_SUCCESS {
            return Cpu::default();
        }
        if info.is_null() {
            return Cpu::default();
        }
        let ticks = std::slice::from_raw_parts(info, length as usize);
        let multiplier = 1000 / libc::sysconf(libc::_SC_CLK_TCK).max(1);
        let mut cpu = Cpu {
            count: u64::from(count),
            ..Cpu::default()
        };
        for processor in ticks.chunks_exact(4).take(count as usize) {
            // These are unsigned cpu_ticks_t even though processor_info uses int*.
            cpu.idle +=
                f64::from(processor[libc::CPU_STATE_IDLE as usize] as u32) * multiplier as f64;
            cpu.total += processor
                .iter()
                .map(|tick| f64::from(*tick as u32) * multiplier as f64)
                .sum::<f64>();
        }
        libc::vm_deallocate(
            libc::mach_task_self(),
            info as libc::vm_address_t,
            length as libc::vm_size_t * std::mem::size_of::<libc::integer_t>() as libc::vm_size_t,
        );
        cpu
    }
}
#[cfg(target_os = "macos")]
pub fn memory() -> Memory {
    unsafe {
        let mut total: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        let name = c"hw.memsize";
        if libc::sysctlbyname(
            name.as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            total = 0;
        }
        let host = HostPort::new();
        let mut info: libc::vm_statistics = std::mem::zeroed();
        let mut count = (std::mem::size_of::<libc::vm_statistics>()
            / std::mem::size_of::<libc::integer_t>()) as u32;
        let available = if libc::host_statistics(
            host.0,
            libc::HOST_VM_INFO,
            (&mut info as *mut libc::vm_statistics).cast(),
            &mut count,
        ) == libc::KERN_SUCCESS
        {
            f64::from(info.free_count) * libc::sysconf(libc::_SC_PAGESIZE) as f64
        } else {
            0.
        };
        Memory {
            total: total as f64,
            available,
        }
    }
}
#[cfg(target_os = "linux")]
pub fn cpu() -> Cpu {
    let output = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut cpu = Cpu::default();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let name = fields.next().unwrap_or("");
        if !name
            .strip_prefix("cpu")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        let values: Vec<f64> = fields
            .take(6)
            .filter_map(|value| value.parse::<f64>().ok())
            .collect();
        if values.len() != 6 {
            continue;
        }
        cpu.count += 1;
        // libuv ignores iowait, softirq, steal and guest counters.
        cpu.idle += values[3] * 10.;
        cpu.total += (values[0] + values[1] + values[2] + values[3] + values[5]) * 10.;
    }
    cpu
}
#[cfg(target_os = "linux")]
pub fn memory() -> Memory {
    unsafe {
        let mut info: libc::sysinfo = std::mem::zeroed();
        if libc::sysinfo(&mut info) != 0 {
            return Memory::default();
        }
        let proc_total = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    let rest = line.strip_prefix("MemTotal:")?;
                    rest.split_whitespace()
                        .next()?
                        .parse::<f64>()
                        .ok()
                        .map(|kb| kb * 1024.)
                })
            })
            .filter(|total| *total != 0.);
        Memory {
            total: proc_total.unwrap_or(info.totalram as f64 * f64::from(info.mem_unit)),
            available: info.freeram as f64 * f64::from(info.mem_unit),
        }
    }
}
#[cfg(windows)]
mod windows {
    use super::*;
    #[repr(C)]
    struct SystemInfo {
        architecture: u32,
        page_size: u32,
        minimum: *mut std::ffi::c_void,
        maximum: *mut std::ffi::c_void,
        mask: usize,
        processors: u32,
        processor_type: u32,
        granularity: u32,
        level: u16,
        revision: u16,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Processor {
        idle: i64,
        kernel: i64,
        user: i64,
        dpc: i64,
        interrupt: i64,
        interrupt_count: u32,
    }
    #[repr(C)]
    struct MemoryStatus {
        length: u32,
        load: u32,
        total: u64,
        available: u64,
        total_page: u64,
        available_page: u64,
        total_virtual: u64,
        available_virtual: u64,
        extended: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemInfo(info: *mut SystemInfo);
        fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQuerySystemInformation(
            kind: u32,
            buffer: *mut std::ffi::c_void,
            size: u32,
            actual: *mut u32,
        ) -> i32;
    }
    pub(super) fn cpu() -> Cpu {
        unsafe {
            let mut system: SystemInfo = std::mem::zeroed();
            GetSystemInfo(&mut system);
            let mut processors = vec![std::mem::zeroed::<Processor>(); system.processors as usize];
            let size = std::mem::size_of_val(processors.as_slice());
            let mut actual = 0;
            if NtQuerySystemInformation(8, processors.as_mut_ptr().cast(), size as u32, &mut actual)
                < 0
                || actual as usize != size
            {
                return Cpu::default();
            }
            let mut cpu = Cpu {
                count: u64::from(system.processors),
                ..Cpu::default()
            };
            for processor in processors {
                cpu.idle += (processor.idle / 10000) as f64;
                cpu.total += (processor.user / 10000
                    + (processor.kernel - processor.idle) / 10000
                    + processor.idle / 10000
                    + processor.interrupt / 10000) as f64;
            }
            cpu
        }
    }
    pub(super) fn memory() -> Memory {
        unsafe {
            let mut status: MemoryStatus = std::mem::zeroed();
            status.length = std::mem::size_of::<MemoryStatus>() as u32;
            if GlobalMemoryStatusEx(&mut status) == 0 {
                Memory::default()
            } else {
                Memory {
                    total: status.total as f64,
                    available: status.available as f64,
                }
            }
        }
    }
}
#[cfg(windows)]
pub fn cpu() -> Cpu {
    windows::cpu()
}
#[cfg(windows)]
pub fn memory() -> Memory {
    windows::memory()
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn cpu() -> Cpu {
    Cpu::default()
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn memory() -> Memory {
    Memory::default()
}
pub fn available() -> crate::host_resources::AvailableMemory {
    use crate::host_resources::AvailableMemory;
    #[cfg(target_os = "linux")]
    {
        return AvailableMemory::value(Box::pin(async {
            crate::host_resources::linux_available_memory(
                &tokio::fs::read_to_string("/proc/meminfo").await.ok()?,
            )
        }));
    }
    #[cfg(target_os = "macos")]
    {
        let command = crate::terminal_inspector::NativeProcessTable::command(
            "/usr/bin/vm_stat".into(),
            vec![],
            "vm_stat",
            std::time::Duration::from_secs(1),
            1_048_576,
        );
        let read = command.clone();
        return AvailableMemory {
            read: Box::pin(crate::host_resources::vm_stat(read)),
            cleanup: Box::pin(async move {
                command.shutdown().await;
            }),
        };
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        AvailableMemory::value(Box::pin(async { None }))
    }
}
