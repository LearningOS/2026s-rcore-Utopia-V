//! RISC-V timer-related functionality

use crate::config::CLOCK_FREQ;
use crate::sbi::set_timer;
use crate::task::{wakeup_task, TaskControlBlock};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use riscv::register::time;
/// The number of ticks per second
const TICKS_PER_SEC: usize = 100;
/// The number of milliseconds per second
const MSEC_PER_SEC: usize = 1000;
/// The number of microseconds per second
const MICRO_PER_SEC: usize = 1_000_000;

/// Get the current time in ticks
pub fn get_time() -> usize {
    time::read()
}

/// get current time in milliseconds
pub fn get_time_ms() -> usize {
    time::read() * MSEC_PER_SEC / CLOCK_FREQ
}

/// get current time in microseconds
pub fn get_time_us() -> usize {
    time::read() * MICRO_PER_SEC / CLOCK_FREQ
}

/// Set the next timer interrupt
pub fn set_next_trigger() {
    set_timer(get_time() + CLOCK_FREQ / TICKS_PER_SEC);
}

/// 定时器队列：到期时间 → 等待唤醒的任务列表
///
/// 用 BTreeMap 按时间排序，每次时钟中断只检查最早的几个条目
static mut TIMER_QUEUE: Option<BTreeMap<usize, Vec<Arc<TaskControlBlock>>>> = None;

fn timer_queue() -> &'static mut BTreeMap<usize, Vec<Arc<TaskControlBlock>>> {
    unsafe { TIMER_QUEUE.get_or_insert_with(BTreeMap::new) }
}

/// 注册定时器：任务在 expire_ms 时刻被唤醒
pub fn add_timer(expire_ms: usize, task: Arc<TaskControlBlock>) {
    timer_queue()
        .entry(expire_ms)
        .or_default()
        .push(task);
}

/// 检查并唤醒所有已到期的定时器任务
///
/// 应在每次时钟中断时调用
pub fn check_timer() {
    let current_ms = get_time_ms();
    let queue = timer_queue();
    // 收集所有已到期的时刻
    let expired: Vec<_> = queue
        .keys()
        .filter(|&&t| t <= current_ms)
        .cloned()
        .collect();
    // 唤醒并移除
    for t in expired {
        if let Some(tasks) = queue.remove(&t) {
            for task in tasks {
                wakeup_task(task);
            }
        }
    }
}
