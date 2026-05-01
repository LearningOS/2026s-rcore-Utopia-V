use crate::{
    task::{TaskStatus, add_task, current_task, TaskControlBlock},
};
/// thread create syscall
pub fn sys_thread_create(entry: usize, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_thread_create",
        current_task().unwrap().process.pid.0,
        current_task().unwrap().tid.0
    );
    let task = current_task().unwrap();
    let process = task.process.clone();
    let new_task = TaskControlBlock::new_thread(process, entry, arg);
    add_task(new_task.clone());
    new_task.tid.0 as isize
}
/// get current thread id syscall
pub fn sys_gettid() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_gettid",
        current_task().unwrap().process.pid.0,
        current_task().unwrap().tid.0
    );
    current_task().unwrap().tid.0 as isize
}

/// wait for a thread to exit syscall
///
/// thread does not exist, return -1
/// thread has not exited yet, return -2
/// otherwise, return thread's exit code
pub fn sys_waittid(tid: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_waittid",
        current_task().unwrap().process.pid.0,
        current_task().unwrap().tid.0
    );
    let task = current_task().unwrap();
    if task.tid.0 == tid { return -1; }

    let mut inner = task.process.inner_exclusive_access();

    if tid >= inner.tasks.len() || inner.tasks[tid].is_none() {
        return -1;
    }

    let waited_task = inner.tasks[tid].as_ref().unwrap();
    let waited_inner = waited_task.inner_exclusive_access();

    if waited_inner.task_status == TaskStatus::Zombie {
        let exit_code = waited_inner.exit_code;
        drop(waited_inner);
        inner.tasks[tid] = None;
        exit_code as isize
    } else {
        -2
    }
}
