// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2022-2023 SUSE LLC
//
// Author: Roy Hopkins <rhopkins@suse.de>

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::fmt;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::address::{Address, VirtAddr};
use crate::cpu::msr::{rdtsc, read_flags};
use crate::cpu::percpu::{this_cpu, this_cpu_mut};
use crate::cpu::X86GeneralRegs;
use crate::error::SvsmError;
use crate::locking::SpinLock;
use crate::mm::pagetable::{get_init_pgtable_locked, PTEntryFlags, PageTableRef};
use crate::mm::stack::StackBounds;
use crate::mm::vm::{Mapping, VMKernelStack, VMR};
use crate::mm::{SVSM_PERTASK_BASE, SVSM_PERTASK_END, SVSM_PERTASK_STACK_BASE};

use super::schedule::{current_task_terminated, schedule};

pub const INITIAL_TASK_ID: u32 = 1;

#[derive(PartialEq, Debug, Copy, Clone, Default)]
pub enum TaskState {
    RUNNING,
    #[default]
    TERMINATED,
}

#[derive(Clone, Copy, Debug)]
pub enum TaskError {
    // Attempt to close a non-terminated task
    NotTerminated,
    // A closed task could not be removed from the task list
    CloseFailed,
}

impl From<TaskError> for SvsmError {
    fn from(e: TaskError) -> Self {
        Self::Task(e)
    }
}

pub const TASK_FLAG_SHARE_PT: u16 = 0x01;

#[derive(Debug, Default)]
struct TaskIDAllocator {
    next_id: AtomicU32,
}

impl TaskIDAllocator {
    const fn new() -> Self {
        Self {
            next_id: AtomicU32::new(INITIAL_TASK_ID + 1),
        }
    }

    fn next_id(&self) -> u32 {
        let mut id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Reserve IDs of 0 and 1
        while (id == 0_u32) || (id == INITIAL_TASK_ID) {
            id = self.next_id.fetch_add(1, Ordering::Relaxed);
        }
        id
    }
}

static TASK_ID_ALLOCATOR: TaskIDAllocator = TaskIDAllocator::new();

/// This trait is used to implement the strategy that determines
/// how much CPU time a task has been allocated. The task with the
/// lowest runtime value is likely to be the next scheduled task
pub trait TaskRuntime {
    /// Called when a task is allocated to a CPU just before the task
    /// context is restored. The task should start tracking the CPU
    /// execution allocation at this point.
    fn schedule_in(&mut self);

    /// Called by the scheduler at the point the task is interrupted
    /// and marked for deallocation from the CPU. The task should
    /// update the runtime calculation at this point.
    fn schedule_out(&mut self);

    /// Overrides the calculated runtime value with the given value.
    /// This can be used to set or adjust the runtime of a task.
    fn set(&mut self, runtime: u64);

    /// Returns a value that represents the amount of CPU the task
    /// has been allocated
    fn value(&self) -> u64;
}

/// Tracks task runtime based on the CPU timestamp counter
#[derive(Default, Debug)]
#[repr(transparent)]
pub struct TscRuntime {
    runtime: u64,
}

impl TaskRuntime for TscRuntime {
    fn schedule_in(&mut self) {
        self.runtime = rdtsc();
    }

    fn schedule_out(&mut self) {
        self.runtime += rdtsc() - self.runtime;
    }

    fn set(&mut self, runtime: u64) {
        self.runtime = runtime;
    }

    fn value(&self) -> u64 {
        self.runtime
    }
}

/// Tracks task runtime based on the number of times the task has been
/// scheduled
#[derive(Default, Debug, Clone, Copy)]
#[repr(transparent)]
pub struct CountRuntime {
    count: u64,
}

impl TaskRuntime for CountRuntime {
    fn schedule_in(&mut self) {
        self.count += 1;
    }

    fn schedule_out(&mut self) {}

    fn set(&mut self, runtime: u64) {
        self.count = runtime;
    }

    fn value(&self) -> u64 {
        self.count
    }
}

// Define which runtime counter to use
type TaskRuntimeImpl = CountRuntime;

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct TaskContext {
    pub rsp: u64,
    pub regs: X86GeneralRegs,
    pub flags: u64,
    pub ret_addr: u64,
}

#[repr(C)]
pub struct Task {
    pub rsp: u64,

    stack_bounds: StackBounds,

    /// Page table that is loaded when the task is scheduled
    pub page_table: SpinLock<PageTableRef>,

    /// Task virtual memory range for use at CPL 0
    vm_kernel_range: VMR,

    /// Whether this is an idle task
    idle_task: bool,

    /// Current state of the task
    pub state: TaskState,

    /// ID of the task
    pub id: u32,

    /// Amount of CPU resource the task has consumed
    pub runtime: TaskRuntimeImpl,
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("rsp", &self.rsp)
            .field("state", &self.state)
            .field("id", &self.id)
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl Task {
    pub fn create(entry: extern "C" fn(), flags: u16) -> Result<Box<Task>, SvsmError> {
        let mut pgtable = if (flags & TASK_FLAG_SHARE_PT) != 0 {
            this_cpu().get_pgtable().clone_shared()?
        } else {
            Self::allocate_page_table()?
        };

        let mut vm_kernel_range =
            VMR::new(SVSM_PERTASK_BASE, SVSM_PERTASK_END, PTEntryFlags::empty());
        vm_kernel_range.initialize()?;

        let (stack, raw_bounds, rsp_offset) = Self::allocate_stack(entry)?;
        vm_kernel_range.insert_at(SVSM_PERTASK_STACK_BASE, stack)?;

        vm_kernel_range.populate(&mut pgtable);

        let bounds = raw_bounds.map_at(SVSM_PERTASK_STACK_BASE);

        let task: Box<Task> = Box::new(Task {
            rsp: bounds
                .top
                .checked_sub(rsp_offset)
                .expect("Invalid stack offset from task::allocate_stack()")
                .bits() as u64,
            stack_bounds: bounds,
            page_table: SpinLock::new(pgtable),
            vm_kernel_range,
            idle_task: false,
            state: TaskState::RUNNING,
            id: TASK_ID_ALLOCATOR.next_id(),
            runtime: TaskRuntimeImpl::default(),
        });
        Ok(task)
    }

    pub fn stack_bounds(&self) -> StackBounds {
        self.stack_bounds
    }

    pub fn set_idle_task(&mut self) {
        self.idle_task = true;
    }

    pub fn is_idle_task(&self) -> bool {
        self.idle_task
    }

    pub fn handle_pf(&self, vaddr: VirtAddr, write: bool) -> Result<(), SvsmError> {
        self.vm_kernel_range.handle_page_fault(vaddr, write)
    }

    fn allocate_stack(
        entry: extern "C" fn(),
    ) -> Result<(Arc<Mapping>, StackBounds, usize), SvsmError> {
        let stack = VMKernelStack::new()?;
        let bounds = stack.bounds(VirtAddr::from(0u64));

        let mapping = Arc::new(Mapping::new(stack));
        let percpu_mapping = this_cpu_mut().new_mapping(mapping.clone())?;

        // We need to setup a context on the stack that matches the stack layout
        // defined in switch_context below.
        let stack_ptr: *mut u64 =
            (percpu_mapping.virt_addr().bits() + bounds.top.bits()) as *mut u64;

        // 'Push' the task frame onto the stack
        unsafe {
            // flags
            stack_ptr.offset(-3).write(read_flags());
            // ret_addr
            stack_ptr.offset(-2).write(entry as *const () as u64);
            // Task termination handler for when entry point returns
            stack_ptr.offset(-1).write(task_exit as *const () as u64);
        }

        Ok((mapping, bounds, size_of::<TaskContext>() + size_of::<u64>()))
    }

    fn allocate_page_table() -> Result<PageTableRef, SvsmError> {
        // Base the new task page table on the initial SVSM kernel page table.
        // When the pagetable is schedule to a CPU, the per CPU entry will also
        // be added to the pagetable.
        get_init_pgtable_locked().clone_shared()
    }
}

extern "C" fn task_exit() {
    unsafe {
        current_task_terminated();
    }
    schedule();
}

#[allow(unused)]
#[no_mangle]
extern "C" fn apply_new_context(new_task: *mut Task) -> u64 {
    unsafe {
        let mut pt = (*new_task).page_table.lock();
        this_cpu().populate_page_table(&mut pt);
        pt.cr3_value().bits() as u64
    }
}
