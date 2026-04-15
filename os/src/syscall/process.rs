//! Process management syscalls
use crate::{mm::{PageTable, PTEFlags, VirtAddr, translated_byte_buffer}, task::{change_program_brk, current_user_token, exit_current_and_run_next, get_syscall_count, mmap_current, munmap, suspend_current_and_run_next}, timer::get_time_us};

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}


pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    let time_us = get_time_us();
    let sec = time_us / 1_000_000;
    let usec = time_us % 1_000_000;
    let tv = TimeVal {
        sec,
        usec,
    };
    let tv_bytes = unsafe {
        core::slice::from_raw_parts(
            &tv as *const TimeVal as *const u8, 
            core::mem::size_of::<TimeVal>(),
        )
    };
    let slices = translated_byte_buffer(current_user_token(), ts as *const u8, core::mem::size_of::<TimeVal>(),);

    let mut offset = 0;
    for slice in slices {
       slice.copy_from_slice(&tv_bytes[offset..offset + slice.len()]);
       offset += slice.len(); 
    }
    0
}


pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");
    match trace_request {
        0 | 1 => {
            let token = current_user_token();
            let page_table = PageTable::from_token(token);
            let vpn = VirtAddr::from(id).floor();
            let offset = VirtAddr::from(id).page_offset();

            match page_table.translate(vpn) {
                None => -1,
                Some(pte) => {
                    let is_user = (pte.flags() & PTEFlags::U) != PTEFlags::empty();
                    match trace_request {
                        0 if is_user && pte.readable() => {
                            pte.ppn().get_bytes_array()[offset] as isize
                        }
                        1 if is_user && pte.writable() => {
                            pte.ppn().get_bytes_array()[offset] = data as u8;
                            0
                        }
                        _ => -1,
                    }
                }
            }
        }
        2 => get_syscall_count(id) as isize,
        _ => -1,
    }
}

pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel: sys_mmap");
    mmap_current(start, len, port)
}

// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    munmap(start, len)
}
/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
