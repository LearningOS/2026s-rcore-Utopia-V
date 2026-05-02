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

    // Note: sem.up() may wake a blocked thread; the woken thread's
    // sem_wait_for will be cleared when it returns from sem.down().
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
        let sem_count = process_inner.semaphore_list.len();
        let thread_count = process_inner.tasks.len();
        ensure_sem_alloc(&mut process_inner.sem_alloc, thread_count, sem_count);
        ensure_sem_wait_for(&mut process_inner.sem_wait_for, thread_count);

        let available = sem.inner.exclusive_access().count;

        if available <= 0 {
            // Would block — tentatively mark as waiting
            process_inner.sem_wait_for[current_tid] = Some(sem_id);

            // Banker's algorithm safety check
            let avail: Vec<isize> = process_inner
                .semaphore_list
                .iter()
                .map(|s| s.as_ref().map_or(0, |sem| sem.inner.exclusive_access().count.max(0)))
                .collect();

            if !is_safe_state(
                &process_inner.sem_alloc,
                &process_inner.sem_wait_for,
                &avail,
            ) {
                // Unsafe — deadlock would occur
                process_inner.sem_wait_for[current_tid] = None;
                drop(process_inner);
                return -0xdead;
            }

            // Safe — proceed to block (allocation recorded after wakeup)
            drop(process_inner);
            sem.down();
            // Woken up — now we actually hold the resource
            let mut process_inner = process.inner_exclusive_access();
            process_inner.sem_alloc[current_tid][sem_id] += 1;
            process_inner.sem_wait_for[current_tid] = None;
            return 0;
        }

        // Won't block — record allocation immediately
        process_inner.sem_alloc[current_tid][sem_id] += 1;
        drop(process_inner);
        sem.down();
        return 0;
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

/// Ensure sem_wait_for vector is large enough
fn ensure_sem_wait_for(sem_wait_for: &mut Vec<Option<usize>>, thread_count: usize) {
    while sem_wait_for.len() < thread_count {
        sem_wait_for.push(None);
    }
}

/// Banker's algorithm safety check.
/// Returns true if the system is in a safe state (no deadlock).
///
/// - avail: currently available units of each semaphore (max(count, 0))
/// - sem_alloc: resources each thread actually holds
/// - sem_wait_for: which semaphore each thread is blocked on
/// - current_tid: the thread about to block (already marked in sem_wait_for)
fn is_safe_state(
    sem_alloc: &[Vec<isize>],
    sem_wait_for: &[Option<usize>],
    avail: &[isize],
) -> bool {
    let thread_count = sem_alloc.len();
    let sem_count = avail.len();
    let mut work = avail.to_vec();
    let mut finish = alloc::vec![false; thread_count];

    // Step 1: unblocked threads can always complete — release their allocation
    // A thread is considered "not blocked" if:
    //   - it has no sem_wait_for entry, OR
    //   - the semaphore it's waiting for actually has available units
    for i in 0..thread_count {
        let is_blocked = sem_wait_for
            .get(i)
            .and_then(|opt| *opt)
            .map_or(false, |wait_sem| {
                wait_sem >= sem_count || avail[wait_sem] <= 0
            });
        if !is_blocked {
            finish[i] = true;
            for j in 0..sem_count {
                if j < sem_alloc[i].len() {
                    work[j] += sem_alloc[i][j];
                }
            }
        }
    }

    // Step 2: iteratively find blocked threads whose need can be satisfied
    loop {
        let mut found = false;
        for i in 0..thread_count {
            if finish[i] {
                continue;
            }
            // Thread i is blocked — it needs 1 unit of the semaphore it's waiting for
            let need_sem = sem_wait_for.get(i).and_then(|opt| *opt);
            if let Some(need) = need_sem {
                if need < sem_count && work[need] >= 1 {
                    // Can satisfy this thread's need → it will complete and release
                    for j in 0..sem_count {
                        if j < sem_alloc[i].len() {
                            work[j] += sem_alloc[i][j];
                        }
                    }
                    finish[i] = true;
                    found = true;
                }
            }
        }
        if !found {
            break;
        }
    }

    // Step 3: if any blocked thread with allocation is unfinished → unsafe
    for i in 0..thread_count {
        if !finish[i] && sem_alloc.get(i).map_or(false, |a| a.iter().any(|&v| v > 0)) {
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
