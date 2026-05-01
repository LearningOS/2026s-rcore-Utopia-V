use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{block_current_and_run_next, current_task};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;

macro_rules! trace_tid {
    ($name:expr) => {
        trace!(
            "kernel:pid[{}] tid[{}] {}",
            current_task().unwrap().process.pid.0,
            current_task().unwrap().tid.0,
            $name
        );
    };
}

/// sleep syscall
pub fn sys_sleep(ms: usize) -> isize {
    trace_tid!("sys_sleep");
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}

/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace_tid!("sys_mutex_create");
    let process = current_task().unwrap().process.clone();
    let mutex: Option<Arc<dyn Mutex + Send + Sync>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() as isize - 1
    }
}

/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace_tid!("sys_mutex_lock");
    let process = current_task().unwrap().process.clone();
    let process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    mutex.lock();
    0
}

/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace_tid!("sys_mutex_unlock");
    let process = current_task().unwrap().process.clone();
    let process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    mutex.unlock();
    0
}

/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace_tid!("sys_semaphore_create");
    let process = current_task().unwrap().process.clone();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_list.len() - 1
    };
    id as isize
}

/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace_tid!("sys_semaphore_up");
    let process = current_task().unwrap().process.clone();
    let process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    sem.up();
    0
}

/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace_tid!("sys_semaphore_down");
    let process = current_task().unwrap().process.clone();
    let process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    sem.down();
    0
}

/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace_tid!("sys_condvar_create");
    let process = current_task().unwrap().process.clone();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}

/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace_tid!("sys_condvar_signal");
    let process = current_task().unwrap().process.clone();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}

/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace_tid!("sys_condvar_wait");
    let process = current_task().unwrap().process.clone();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}

/// enable deadlock detection syscall (not implemented)
pub fn sys_enable_deadlock_detect(_enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect NOT IMPLEMENTED");
    -1
}
