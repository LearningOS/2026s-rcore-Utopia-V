use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{block_current_and_run_next, current_task};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;
use alloc::vec::Vec;

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
    let deadlock_detect = process_inner.deadlock_detect_enabled;
    drop(process_inner);
    // Deadlock detection: same thread locking mutex it already holds
    if deadlock_detect && mutex.is_held_by_current() {
        return -0xdead;
    }
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
    let current_tid = current_task().unwrap().tid.0;
    let process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    let deadlock_detect = process_inner.deadlock_detect_enabled;
    drop(process_inner);

    if deadlock_detect {
        let mut process_inner = process.inner_exclusive_access();
        let sem_count = process_inner.semaphore_list.len();
        let thread_count = process_inner.tasks.len();
        ensure_sem_alloc(&mut process_inner.sem_alloc, thread_count, sem_count);
        if current_tid < process_inner.sem_alloc.len() && process_inner.sem_alloc[current_tid][sem_id] > 0 {
            process_inner.sem_alloc[current_tid][sem_id] -= 1;
        }
    }

    sem.up();
    0
}

/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace_tid!("sys_semaphore_down");
    let process = current_task().unwrap().process.clone();
    let current_tid = current_task().unwrap().tid.0;

    let mut process_inner = process.inner_exclusive_access();
    let deadlock_detect = process_inner.deadlock_detect_enabled;
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());

    if deadlock_detect {
        // Simulate allocation: check if granting this request is safe
        let sem_inner = sem.inner.exclusive_access();
        let available = sem_inner.count;
        drop(sem_inner);

        // If available > 0, semaphore won't block, always safe
        if available <= 0 {
            // Would block — check if this creates deadlock
            // Build the allocation state for Banker's algorithm
            let sem_count = process_inner.semaphore_list.len();
            let thread_count = process_inner.tasks.len();

            // Ensure sem_alloc is large enough
            ensure_sem_alloc(&mut process_inner.sem_alloc, thread_count, sem_count);

            // Find all "running" threads (need resources)
            // Available resources = current semaphore counts (would be after decrement)
            let mut avail: Vec<isize> = Vec::new();
            for s in &process_inner.semaphore_list {
                if let Some(sem) = s {
                    avail.push(sem.inner.exclusive_access().count);
                } else {
                    avail.push(0);
                }
            }

            // Simulate: current thread requests 1 unit of sem_id
            avail[sem_id] -= 1;
            process_inner.sem_alloc[current_tid][sem_id] += 1;

            if !is_safe(&avail, &process_inner.sem_alloc, thread_count) {
                // Unsafe — rollback and return deadlock error
                process_inner.sem_alloc[current_tid][sem_id] -= 1;
                drop(process_inner);
                return -0xdead;
            }
            // Safe — proceed with the actual down (allocation already recorded)
            drop(process_inner);
            sem.down();
            return 0;
        }
    }

    // Not deadlock detect, or available > 0 (won't block)
    if deadlock_detect {
        let sem_count = process_inner.semaphore_list.len();
        let thread_count = process_inner.tasks.len();
        ensure_sem_alloc(&mut process_inner.sem_alloc, thread_count, sem_count);
        process_inner.sem_alloc[current_tid][sem_id] += 1;
    }
    drop(process_inner);
    sem.down();
    0
}

/// Ensure sem_alloc matrix is large enough
fn ensure_sem_alloc(sem_alloc: &mut Vec<Vec<isize>>, thread_count: usize, sem_count: usize) {
    while sem_alloc.len() < thread_count {
        sem_alloc.push(alloc::vec![0; sem_count]);
    }
    for row in sem_alloc.iter_mut() {
        while row.len() < sem_count {
            row.push(0);
        }
    }
}

/// Banker's algorithm safety check
fn is_safe(
    available: &[isize],
    allocation: &[Vec<isize>],
    thread_count: usize,
) -> bool {
    if thread_count == 0 || allocation.is_empty() {
        return true;
    }
    let sem_count = available.len();
    let mut work = available.to_vec();
    let mut finish: Vec<bool> = (0..thread_count).map(|_| false).collect();

    loop {
        let mut found = false;
        for i in 0..thread_count {
            if finish[i] {
                continue;
            }
            let can_proceed = (0..sem_count).all(|j| work[j] >= 0);
            if can_proceed && i < allocation.len() && allocation[i].iter().any(|&v| v > 0) {
                for j in 0..sem_count {
                    work[j] += allocation[i][j];
                }
                finish[i] = true;
                found = true;
            }
        }
        if !found {
            break;
        }
    }

    // If any thread with allocation is not finished, it's unsafe
    for i in 0..thread_count {
        if !finish[i] && i < allocation.len() && allocation[i].iter().any(|&v| v > 0) {
            return false;
        }
    }
    true
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

/// enable deadlock detection syscall
pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    trace_tid!("sys_enable_deadlock_detect");
    let process = current_task().unwrap().process.clone();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.deadlock_detect_enabled = enabled != 0;
    0
}
