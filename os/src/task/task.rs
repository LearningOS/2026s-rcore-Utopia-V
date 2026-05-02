//! 任务管理相关类型 & 完全切换 TCB 的函数

use super::{kstack_alloc, pid_alloc, KernelStack, PidHandle, SignalActions, SignalFlags, TaskContext};
use crate::{
    config::{PAGE_SIZE, TRAP_CONTEXT_BASE, USER_STACK_SIZE}, fs::{File, Stdin, Stdout}, mm::{KERNEL_SPACE, MapPermission, MemorySet, PhysPageNum, VirtAddr, kernel_token, translated_refmut}, sync::{Condvar, Mutex, Semaphore, UPSafeCell}, task::id::TidHandle, trap::{TrapContext, trap_handler}
};
use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::cell::RefMut;

/// 任务控制块
///
/// 保存运行期间不变的内容
pub struct TaskControlBlock {
    // 不可变
    /// 进程标识符
    pub tid: TidHandle,

    /// 对应 PID 的内核栈
    pub kernel_stack: KernelStack,

    // 可变
    inner: UPSafeCell<TaskControlBlockInner>,

    /// 所属进程
    pub process: Arc<ProcessControlBlock>,
}

impl TaskControlBlock {
    /// 获取 TCB inner 的可变引用
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// 获取应用页表地址
    pub fn get_user_token(&self) -> usize {
        let inner = self.process.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct ProcessControlBlock {
    pub pid: PidHandle,
    inner: UPSafeCell<ProcessControlBlockInner>,
}

impl ProcessControlBlock {
    pub fn inner_exclusive_access(&self) -> RefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }
    pub fn get_user_token(&self) -> usize {
        let inner = self.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct TaskControlBlockInner {
    /// trap context 所在页帧的物理页号
    pub trap_cx_ppn: PhysPageNum,

    /// trap context 的虚拟地址（用于 trap_return 传给 __restore）
    pub trap_cx_va: usize,

    /// 保存任务上下文
    pub task_cx: TaskContext,

    /// 当前进程的执行状态
    pub task_status: TaskStatus,

    /// 主动退出或执行出错时设置
    pub exit_code: i32,

    /// Stride 调度优先级
    pub priority: usize,
    /// Stride 调度步长
    pub stride: usize,
}

pub struct ProcessControlBlockInner {
    /// 应用地址空间
    pub memory_set: MemorySet,

    /// 应用数据只能出现在低于 base_size 的地址空间区域
    pub base_size: usize,

    /// 当前进程的父进程
    /// Weak 不会影响父进程的引用计数
    pub parent: Option<Weak<TaskControlBlock>>,

    /// 当前进程所有子进程的 TCB 向量
    pub children: Vec<Arc<TaskControlBlock>>,
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    pub signals: SignalFlags,
    pub signal_mask: SignalFlags,
    // 正在处理的信号
    pub handling_sig: isize,
    // 信号处理动作
    pub signal_actions: SignalActions,
    // 任务是否已被 kill
    pub killed: bool,
    // 任务是否被信号冻结
    pub frozen: bool,
    pub trap_ctx_backup: Option<TrapContext>,

    /// 堆底
    pub heap_bottom: usize,

    /// 程序断点
    pub program_brk: usize,

    /// 同进程下的所有线程（用 tid 索引）
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,

    /// 进程内的信号量
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,

    /// 进程内的互斥锁
    pub mutex_list: Vec<Option<Arc<dyn Mutex + Send + Sync>>>,

    /// 进程内的条件变量
    pub condvar_list: Vec<Option<Arc<Condvar>>>,

    /// 用户栈
    pub next_ustack: usize,

    /// 下一个可用的线程 TID（进程内）
    pub next_tid: usize,

    /// 死锁检测开关
    pub deadlock_detect_enabled: bool,

    /// 每个线程持有的信号量资源分配矩阵
    /// tid -> sem_id -> 持有数量
    pub sem_alloc: Vec<Vec<isize>>,

    /// 每个线程正在等待的信号量（用于死锁检测）
    /// tid -> Some(sem_id) 表示该线程阻塞在 sem_id 上
    pub sem_wait_for: Vec<Option<usize>>,
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
    pub fn is_zombie(&self) -> bool {
        self.get_status() == TaskStatus::Zombie
    }
}

impl ProcessControlBlockInner {
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    pub fn alloc_fd(&mut self) -> usize {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
    }
}

impl TaskControlBlock {
    /// 创建新进程
    ///
    /// 目前仅用于 initproc 的创建
    pub fn new(elf_data: &[u8]) -> Self {
        // 从 ELF 创建地址空间（含程序头/trampoline/trap context/用户栈）
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // 在内核空间分配 PID 和内核栈
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        // 在内核栈顶压入一个跳转到 trap_return 的任务上下文
        let process_control_block = Arc::new(ProcessControlBlock {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    memory_set,
                    base_size: user_sp,
                    parent: None,
                    children: Vec::new(),
                    fd_table: vec![
                        Some(Arc::new(Stdin)),
                        Some(Arc::new(Stdout)),
                        Some(Arc::new(Stdout))
                    ],
                    signals: SignalFlags::empty(),
                    signal_mask: SignalFlags::empty(),
                    handling_sig: -1,
                    signal_actions: SignalActions::default(),
                    killed: false,
                    frozen: false,
                    trap_ctx_backup: None,
                    heap_bottom: user_sp,
                    program_brk: user_sp,
                    tasks: Vec::new(),
                    semaphore_list: Vec::new(),
                    mutex_list: Vec::new(),
                    condvar_list: Vec::new(),
                    next_ustack: user_sp,
                    next_tid: 1,
                    deadlock_detect_enabled: false,
                    sem_alloc: Vec::new(),
                    sem_wait_for: Vec::new(),
                })
            }
        });
        let task_control_block = Self {
            tid: TidHandle(0),
            kernel_stack,
            process: process_control_block.clone(),
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    trap_cx_va: TRAP_CONTEXT_BASE,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    exit_code: 0,
                    priority: 16,
                    stride: 0,
                })
            },
        };
        // 在用户空间准备 TrapContext
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        task_control_block
    }

    /// 加载新 ELF 替换原应用地址空间并开始执行
    pub fn exec(&self, elf_data: &[u8], args: Vec<String>) {
        // 从 ELF 创建地址空间（含程序头/trampoline/trap context/用户栈）
        let (memory_set, mut user_sp, entry_point) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // 保存原始 user_sp（用于 heap/线程栈起始地址）
        let original_user_sp = user_sp;
        // 在用户栈上压入参数
        user_sp -= (args.len() + 1) * core::mem::size_of::<usize>();
        let argv_base = user_sp;
        let mut argv: Vec<_> = (0..=args.len())
            .map(|arg| {
                translated_refmut(
                    memory_set.token(),
                    (argv_base + arg * core::mem::size_of::<usize>()) as *mut usize,
                )
            })
            .collect();
        *argv[args.len()] = 0;
        for i in 0..args.len() {
            user_sp -= args[i].len() + 1;
            *argv[i] = user_sp;
            let mut p = user_sp;
            for c in args[i].as_bytes() {
                *translated_refmut(memory_set.token(), p as *mut u8) = *c;
                p += 1;
            }
            *translated_refmut(memory_set.token(), p as *mut u8) = 0;
        }
        // 将 user_sp 对齐到 8 字节（k210 平台要求）
        user_sp -= user_sp % core::mem::size_of::<usize>();

        // 替换 memory_set 并重置进程字段
        {
            let mut process_inner = self.process.inner_exclusive_access();
            process_inner.memory_set = memory_set;
            process_inner.heap_bottom = original_user_sp;
            process_inner.program_brk = original_user_sp;
            process_inner.next_ustack = original_user_sp;
            process_inner.next_tid = 1;
            process_inner.deadlock_detect_enabled = false;
            process_inner.sem_alloc = Vec::new();
            process_inner.sem_wait_for = Vec::new();
        }

        // **** 独占访问当前 TCB
        let mut inner = self.inner_exclusive_access();

        // 更新 trap_cx ppn
        inner.trap_cx_ppn = trap_cx_ppn;
        // 初始化 trap_cx
        let mut trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            self.kernel_stack.get_top(),
            trap_handler as usize,
        );
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = argv_base;
        *inner.get_trap_cx() = trap_cx;
        // **** 释放当前 PCB
    }

    /// 从父进程 fork 到子进程
    pub fn fork(self: &Arc<TaskControlBlock>) -> Arc<TaskControlBlock> {
        // ---- 持有父进程 PCB 锁
        let mut parent_inner = self.process.inner_exclusive_access();
        // 拷贝用户空间（含 trap context）
        let memory_set = MemorySet::from_existed_user(&parent_inner.memory_set);
        let trap_cx_ppn = memory_set
            .translate(VirtAddr::from(TRAP_CONTEXT_BASE).into())
            .unwrap()
            .ppn();
        // 在内核空间分配 PID 和内核栈
        let pid_handle = pid_alloc();
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();
        // 拷贝 fd 表
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent_inner.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        let process_control_block = Arc::new(ProcessControlBlock {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    memory_set,
                    base_size: parent_inner.base_size,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    signal_mask: parent_inner.signal_mask,
                    handling_sig: -1,
                    signal_actions: parent_inner.signal_actions.clone(),
                    killed: false,
                    frozen: false,
                    trap_ctx_backup: None,
                    heap_bottom: parent_inner.heap_bottom,
                    program_brk: parent_inner.program_brk,
                    tasks: Vec::new(),
                    semaphore_list: Vec::new(),
                    mutex_list: Vec::new(),
                    condvar_list: Vec::new(),
                    next_ustack: parent_inner.next_ustack,
                    next_tid: 1,
                    deadlock_detect_enabled: false,
                    sem_alloc: Vec::new(),
                    sem_wait_for: Vec::new(),
                })
            }
        });
        let pcb_for_tasks = process_control_block.clone();
        let task_control_block = Arc::new(TaskControlBlock {
            tid: TidHandle(0),
            process: process_control_block,
            kernel_stack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    trap_cx_va: TRAP_CONTEXT_BASE,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    exit_code: 0,
                    priority: 16,
                    stride: 0,
                })
            },
        });
        // 添加子进程
        parent_inner.children.push(task_control_block.clone());
        // 修改 trap_cx 中的 kernel_sp
        // **** 独占访问子进程 PCB
        let trap_cx = task_control_block.inner_exclusive_access().get_trap_cx();
        trap_cx.kernel_sp = kernel_stack_top;
        // 把主线程加入子进程的 tasks 列表
        {
            let mut child_process_inner = pcb_for_tasks.inner_exclusive_access();
            let tid = task_control_block.tid.0;
            while child_process_inner.tasks.len() < tid + 1 {
                child_process_inner.tasks.push(None);
            }
            child_process_inner.tasks[tid] = Some(task_control_block.clone());
        }
        // 返回
        task_control_block
        // **** 释放子进程 PCB
        // ---- 释放父进程 PCB
    }

    /// 线程构造
    pub fn new_thread(process: Arc<ProcessControlBlock>, entry: usize, arg: usize) -> Arc<Self> {
        // 分配资源
        let kernel_stack = kstack_alloc();
        let kernel_stack_top = kernel_stack.get_top();

        // 在共享地址空间里映射 trap context 和用户栈
        let (trap_cx_ppn, trap_cx_va, user_stack_top, tid) = {
            let mut inner = process.inner_exclusive_access();
            let tid = inner.next_tid;
            inner.next_tid += 1;
            let trap_cx_va = TRAP_CONTEXT_BASE - tid * PAGE_SIZE;
            inner.memory_set.insert_framed_area(
                VirtAddr::from(trap_cx_va),
                VirtAddr::from(trap_cx_va + PAGE_SIZE),
                MapPermission::R | MapPermission::W
            );

            let ustack_bottom = inner.next_ustack;
            let ustack_top = ustack_bottom + USER_STACK_SIZE;
            inner.memory_set.insert_framed_area(
                VirtAddr::from(ustack_bottom),
                VirtAddr::from(ustack_top),
                MapPermission::R | MapPermission::W | MapPermission::U
            );
            inner.next_ustack = ustack_top;

            let trap_cx_ppn = inner.memory_set
                .translate(VirtAddr::from(trap_cx_va).into())
                .unwrap()
                .ppn();

            (trap_cx_ppn, trap_cx_va, ustack_top, tid)
        };

        let task = Arc::new(Self {
            tid: TidHandle(tid),
            kernel_stack,
            process: process.clone(),
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    trap_cx_ppn,
                    trap_cx_va,
                    task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                    task_status: TaskStatus::Ready,
                    exit_code: 0,
                    priority: 16,
                    stride: 0
                })
            },
        });

        let trap_cx = task.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry,
            user_stack_top,
            kernel_token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        trap_cx.x[10] = arg;

        {
            let mut inner = process.inner_exclusive_access();
            let tid = task.tid.0;
            while inner.tasks.len() < tid + 1 {
                inner.tasks.push(None);
            }
            inner.tasks[tid] = Some(task.clone());
        }
        task
    }

    /// 获取进程 PID
    pub fn getpid(&self) -> usize {
        self.process.pid.0
    }

    /// 修改 program break 位置，失败返回 None
    pub fn change_program_brk(&self, size: i32) -> Option<usize> {
        let mut inner = self.process.inner_exclusive_access();
        let heap_bottom = inner.heap_bottom;
        let old_break = inner.program_brk;
        let new_brk = inner.program_brk as isize + size as isize;
        if new_brk < heap_bottom as isize {
            return None;
        }
        let result = if size < 0 {
            inner
                .memory_set
                .shrink_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        } else {
            inner
                .memory_set
                .append_to(VirtAddr(heap_bottom), VirtAddr(new_brk as usize))
        };
        if result {
            inner.program_brk = new_brk as usize;
            Some(old_break)
        } else {
            None
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
/// 任务状态: 未初始化, 就绪, 运行中, 已退出
pub enum TaskStatus {
    /// 未初始化
    UnInit,
    /// 就绪
    Ready,
    /// 运行中
    Running,
    /// 已退出
    Zombie,
    /// 阻塞中
    Blocking,
}
